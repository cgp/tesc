//! Bounded ownership transfer: no histogram merge, clone or reset on arrival dispatch.
use crate::{Diagnostics, Lag, Recording, Report, flush};
use metrix_metrics::{
    aggregation::{Accumulator, AuthCounts, Window},
    events::Phase,
};
use metrix_plan::mix::StopOn;
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio::sync::{mpsc as async_mpsc, oneshot};

const BUFFERS: usize = 3;

struct Buffers {
    workers: Vec<Accumulator>,
    warmup_workers: Vec<Accumulator>,
    lag: Lag,
}

pub(crate) struct Cutoff {
    pub phase: Phase,
    pub now: Duration,
    pub active: usize,
    pub queue_depth: usize,
    pub warmup_active: usize,
    pub warmup_queue: usize,
    pub auth: Option<AuthCounts>,
    pub diagnostics: Diagnostics,
    pub closing: bool,
    pub partial: bool,
}

struct Job {
    buffers: Buffers,
    cutoff: Cutoff,
}

pub(crate) struct Completed {
    buffers: Buffers,
    pub window: Arc<Window>,
    pub diagnostics: Diagnostics,
    pub closing: bool,
    pub partial: bool,
    pub stop: Option<String>,
}

pub(crate) struct Aggregation {
    jobs: mpsc::SyncSender<Job>,
    results: async_mpsc::Receiver<Completed>,
    final_report: oneshot::Receiver<Report>,
    spare: Vec<Buffers>,
    pending: usize,
    pub coalesced: u64,
}

impl Aggregation {
    pub fn new(
        workers: &[Accumulator],
        warmup_workers: &[Accumulator],
        mut interval: Accumulator,
        mut warmup_interval: Accumulator,
        snapshots: Option<async_mpsc::Sender<Window>>,
        stop: Option<(StopOn, Option<u64>)>,
    ) -> Result<Self, String> {
        let spare = (1..BUFFERS)
            .map(|_| Buffers {
                workers: workers.to_vec(),
                warmup_workers: warmup_workers.to_vec(),
                lag: Lag::default(),
            })
            .collect();
        let (jobs, receiver) = mpsc::sync_channel::<Job>(BUFFERS);
        let (results_sender, results) = async_mpsc::channel(BUFFERS);
        let (final_sender, final_report) = oneshot::channel();
        std::thread::Builder::new()
            .name("metrix-aggregation".into())
            .spawn(move || {
                let mut report = Report::default();
                let mut last = Duration::ZERO;
                while let Ok(mut job) = receiver.recv() {
                    let cutoff = job.cutoff;
                    report.diagnostics = cutoff.diagnostics;
                    flush(
                        Recording {
                            queue_depth: cutoff.queue_depth,
                            warmup_active: cutoff.warmup_active,
                            warmup_queue: cutoff.warmup_queue,
                            workers: &mut job.buffers.workers,
                            warmup_workers: &mut job.buffers.warmup_workers,
                            warmup_interval: &mut warmup_interval,
                            phase: cutoff.phase,
                            lag: &mut job.buffers.lag,
                            auth: cutoff.auth,
                        },
                        &mut interval,
                        &mut report,
                        &mut last,
                        cutoff.now,
                        cutoff.active,
                        snapshots.as_ref(),
                    );
                    let reason = if cutoff.phase == Phase::Measure && !cutoff.closing {
                        stop.and_then(|(thresholds, baseline)| {
                            crate::breakpoint::assess(&report, thresholds, baseline)
                        })
                    } else {
                        None
                    };
                    let completed = Completed {
                        buffers: job.buffers,
                        window: Arc::clone(report.last_window.as_ref().expect("flushed window")),
                        diagnostics: report.diagnostics,
                        closing: cutoff.closing,
                        partial: cutoff.partial,
                        stop: reason,
                    };
                    // Every queued result owns a buffer set. With only BUFFERS sets,
                    // a BUFFERS-capacity result channel always has room for this one.
                    // A dropped execution future closes the receiver; never wait on it.
                    if results_sender.try_send(completed).is_err() {
                        return;
                    }
                }
                let _ = final_sender.send(report);
            })
            .map_err(|error| format!("cannot start aggregation worker: {error}"))?;
        Ok(Self {
            jobs,
            results,
            final_report,
            spare,
            pending: 0,
            coalesced: 0,
        })
    }

    pub fn available(&self) -> bool {
        !self.spare.is_empty()
    }
    pub fn pending(&self) -> bool {
        self.pending > 0
    }

    pub fn submit(
        &mut self,
        workers: &mut Vec<Accumulator>,
        warmup_workers: &mut Vec<Accumulator>,
        lag: &mut Lag,
        cutoff: Cutoff,
    ) -> Result<(), String> {
        let mut buffers = self.spare.pop().expect("spare checked before handoff");
        std::mem::swap(workers, &mut buffers.workers);
        std::mem::swap(warmup_workers, &mut buffers.warmup_workers);
        std::mem::swap(lag, &mut buffers.lag);
        self.jobs
            .try_send(Job { buffers, cutoff })
            .map_err(|_| "aggregation worker stopped".to_owned())?;
        self.pending += 1;
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<Completed, String> {
        self.results
            .recv()
            .await
            .ok_or_else(|| "aggregation worker stopped".to_owned())
    }

    pub fn recycle(&mut self, completed: Completed) {
        self.pending -= 1;
        self.spare.push(completed.buffers);
    }

    pub async fn finish(self, report: &mut Report) -> Result<(), String> {
        debug_assert_eq!(self.pending, 0);
        drop(self.jobs);
        let totals = self
            .final_report
            .await
            .map_err(|_| "aggregation worker stopped".to_owned())?;
        report.metrics = totals.metrics;
        report.warmup_metrics = totals.warmup_metrics;
        report.arrival_timing = totals.arrival_timing;
        report.warmup_arrival_timing = totals.warmup_arrival_timing;
        report.snapshot_timing.aggregation = totals.snapshot_timing.aggregation;
        report.snapshot_timing.window_construction = totals.snapshot_timing.window_construction;
        report.snapshot_timing.flush_total = totals.snapshot_timing.flush_total;
        report.windows = totals.windows;
        report.windows_dropped = totals.windows_dropped;
        report
            .diagnostics
            .sync(&report.metrics, &report.warmup_metrics);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DetectorConfig;

    fn cutoff(phase: Phase, ms: u64, offered: u64, skipped: u64) -> Cutoff {
        let mut diagnostics = Diagnostics::new(
            DetectorConfig {
                rate_tolerance_pct: 10,
                drift_threshold: Duration::from_secs(1),
            },
            100,
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_secs(5),
        );
        diagnostics.measure.observed = Duration::from_millis(ms);
        diagnostics.measure.offered = offered;
        diagnostics.measure.skipped_late = skipped;
        Cutoff {
            phase,
            now: Duration::from_millis(ms),
            active: 1,
            queue_depth: 0,
            warmup_active: 1,
            warmup_queue: 0,
            auth: None,
            diagnostics,
            closing: false,
            partial: false,
        }
    }

    #[tokio::test]
    async fn bounded_pool_retains_samples_and_consumer_backpressure_never_blocks() {
        let mut workers = vec![Accumulator::default()];
        let mut warmup = vec![Accumulator::default()];
        let mut lag = Lag::default();
        let (sender, mut receiver) = async_mpsc::channel(1);
        let mut aggregation = Aggregation::new(
            &workers,
            &warmup,
            Accumulator::default(),
            Accumulator::default(),
            Some(sender),
            None,
        )
        .unwrap();
        warmup[0].drift.record(Duration::from_micros(10));
        aggregation
            .submit(
                &mut workers,
                &mut warmup,
                &mut lag,
                cutoff(Phase::Warmup, 250, 0, 0),
            )
            .unwrap();
        workers[0].drift.record(Duration::from_micros(20));
        warmup[0].drift.record(Duration::from_micros(40));
        aggregation
            .submit(
                &mut workers,
                &mut warmup,
                &mut lag,
                cutoff(Phase::Measure, 500, 1, 0),
            )
            .unwrap();
        // Results retain ownership until consumed: capacity cannot grow even if
        // the worker has finished. The active set continues accepting samples.
        assert!(!aggregation.available());
        workers[0].drift.record(Duration::from_micros(30));
        assert_eq!(workers[0].drift.count(), 1);
        let first = aggregation.receive().await.unwrap();
        assert_eq!(first.window.phase, Phase::Warmup);
        assert_eq!(first.window.metrics.drift.count(), 1);
        aggregation.recycle(first);
        aggregation
            .submit(
                &mut workers,
                &mut warmup,
                &mut lag,
                cutoff(Phase::Measure, 1000, 2, 0),
            )
            .unwrap();
        let second = aggregation.receive().await.unwrap();
        assert_eq!(second.window.warmup_in_flight, 1);
        assert_eq!(
            second.window.warmup_metrics.as_ref().unwrap().drift.count(),
            1
        );
        aggregation.recycle(second);
        let third = aggregation.receive().await.unwrap();
        assert_eq!(third.window.from, Duration::from_millis(500));
        assert_eq!(third.window.to, Duration::from_millis(1000));
        assert_eq!(third.window.metrics.drift.count(), 1);
        aggregation.recycle(third);
        let mut report = Report::default();
        aggregation.finish(&mut report).await.unwrap();
        assert_eq!(report.windows, 3);
        assert_eq!(report.windows_dropped, 2);
        assert_eq!(report.metrics.drift.count(), 2);
        assert_eq!(report.warmup_metrics.drift.count(), 2);
        assert_eq!(report.snapshot_timing.aggregation.count(), 3);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn stop_assessments_use_diagnostics_from_each_buffer_cutoff() {
        let mut workers = vec![Accumulator::default()];
        workers[0].declare("test", &[]);
        let mut warmup = vec![Accumulator::default()];
        let mut lag = Lag::default();
        let mut aggregation = Aggregation::new(
            &workers,
            &warmup,
            Accumulator::default(),
            Accumulator::default(),
            None,
            Some((StopOn::default(), None)),
        )
        .unwrap();
        for _ in 0..100 {
            workers[0].start_chain("test");
        }
        aggregation
            .submit(
                &mut workers,
                &mut warmup,
                &mut lag,
                cutoff(Phase::Measure, 1000, 100, 0),
            )
            .unwrap();
        for _ in 0..100 {
            workers[0].start_chain("test");
        }
        aggregation
            .submit(
                &mut workers,
                &mut warmup,
                &mut lag,
                cutoff(Phase::Measure, 1250, 240, 40),
            )
            .unwrap();
        let first = aggregation.receive().await.unwrap();
        assert!(first.stop.is_none());
        assert_eq!(first.diagnostics.measure.offered, 100);
        aggregation.recycle(first);
        let second = aggregation.receive().await.unwrap();
        assert_eq!(second.stop.as_deref(), Some("generator_limited"));
        assert_eq!(second.diagnostics.measure.offered, 240);
        aggregation.recycle(second);
        let mut report = Report::default();
        aggregation.finish(&mut report).await.unwrap();
        assert_eq!(report.metrics.chains["test"].started, 200);
    }
}
