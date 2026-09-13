//! Fixed-rate load execution independent of the control plane.

mod bundle;
mod http;
mod output;
mod schedule;
mod wake_clock;

pub use bundle::Plan;
pub use http::Failure;
use metrix_metrics::events::Phase;
pub use output::{Output, OutputReport};

use metrix_metrics::aggregation::{Accumulator, Cause, Sample, Window};
use std::{
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::ReusableBoxFuture;

use http::{Completion, Job, Pool, SendState, execute};
use schedule::Schedule;
use wake_clock::WakeClock;

#[derive(Debug, Default)]
pub struct Report {
    pub offered: u64,
    pub admitted: u64,
    /// Sends among terminal attempts. In-flight cancellations are counted separately.
    pub sent_finished: u64,
    /// Observed sends, including requests still in flight or subsequently cancelled.
    pub sent: u64,
    pub responses: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    pub skipped_late: u64,
    pub skipped_concurrency: u64,
    pub skipped_connections: u64,
    pub peak_in_flight: usize,
    pub max_send_drift: Duration,
    pub max_scheduler_lag: Duration,
    pub scheduler_lag_samples: u64,
    pub interrupted: bool,
    pub metrics: Accumulator,
    pub last_window: Option<Window>,
    pub windows: u64,
    pub windows_dropped: u64,
}

struct Slot {
    active: bool,
    worker: usize,
    iteration: u64,
    admitted: Instant,
    send_state: Arc<SendState>,
    send_recorded: bool,
    future: ReusableBoxFuture<'static, Completion>,
}

/// The scheduler never writes to stdout/stderr or waits for an output consumer.
pub async fn run(plan: Plan, shutdown: impl Future<Output = ()>) -> Result<Report, String> {
    run_with_snapshots(plan, shutdown, None).await
}

/// Snapshot delivery never blocks on a full or closed consumer channel.
pub async fn run_with_snapshots(
    plan: Plan,
    shutdown: impl Future<Output = ()>,
    snapshots: Option<mpsc::Sender<Window>>,
) -> Result<Report, String> {
    run_internal(plan, shutdown, snapshots, None).await
}

/// NDJSON delivery shares the nonblocking snapshot path; request packets contain only scalars.
pub async fn run_with_output(
    plan: Plan,
    shutdown: impl Future<Output = ()>,
    output: &Output,
) -> Result<Report, String> {
    run_internal(plan, shutdown, None, Some(output)).await
}

async fn run_internal(
    plan: Plan,
    shutdown: impl Future<Output = ()>,
    snapshots: Option<mpsc::Sender<Window>>,
    output: Option<&Output>,
) -> Result<Report, String> {
    tokio::pin!(shutdown);
    let mut report = Report::default();
    let mut pool = tokio::select! {
        biased;
        _ = &mut shutdown => { report.interrupted = true; return Ok(report); }
        pool = Pool::prepare(&plan.target, plan.connections.min(plan.concurrency), plan.request.timeout) => pool.map_err(|e| format!("target setup failed: {e}"))?,
    };
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(plan.concurrency)
        .map_err(|_| "cannot allocate request slots")?;
    for _ in 0..plan.concurrency {
        slots.push(Slot {
            active: false,
            worker: 0,
            iteration: 0,
            admitted: Instant::now(),
            send_state: Arc::new(SendState::default()),
            send_recorded: false,
            future: ReusableBoxFuture::new(execute(None)),
        });
    }
    let mut workers: Vec<_> = (0..plan.worker_threads.min(plan.concurrency))
        .map(|_| Accumulator::default())
        .collect();
    workers[0].counters.connections_opened = 1;
    let mut interval_metrics = Accumulator::default();
    let mut lag = Lag::default();
    let start = Instant::now();
    let mut phase = Phase::Measure;
    if let Some(output) = output {
        output.phase(phase);
    }
    let mut last_snapshot = Duration::ZERO;
    let mut snapshot_tick = tokio::time::interval_at(
        start + Duration::from_millis(250),
        Duration::from_millis(250),
    );
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut schedule = Schedule::new(start, plan.rate, plan.duration);
    let clock = WakeClock::start(start, plan.rate, plan.duration)?;
    let mut clock_tick = 0;
    let mut active = 0;
    while Instant::now() < start + plan.duration || schedule.next_deadline().is_some() || active > 0
    {
        if phase == Phase::Measure
            && Instant::now() >= start + plan.duration
            && schedule.next_deadline().is_none()
        {
            flush(
                Recording {
                    slots: &mut slots,
                    workers: &mut workers,
                    lag: &mut lag,
                },
                &mut interval_metrics,
                &mut report,
                &mut last_snapshot,
                start.elapsed(),
                active,
                snapshots.as_ref(),
            );
            if let Some(output) = output {
                output.summary(report.last_window.as_ref().expect("flushed window"), phase);
                output.phase(Phase::Drain);
            }
            phase = Phase::Drain;
        }
        let next = schedule.next_deadline();
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                report.interrupted = true;
                report.cancelled = active as u64;
                for slot in &slots { if slot.active {
                    workers[slot.worker].cancel();
                    if let Some(output) = output { output.cancel(slot.iteration, phase, slot.admitted.elapsed()); }
                } }
                break;
            }
            deadline = snapshot_tick.tick() => {
                let late = Instant::now().saturating_duration_since(deadline);
                lag.max = lag.max.max(late);
                lag.samples += 1;
                report.max_scheduler_lag = report.max_scheduler_lag.max(late);
                report.scheduler_lag_samples += 1;
                flush(Recording { slots: &mut slots, workers: &mut workers, lag: &mut lag }, &mut interval_metrics, &mut report, &mut last_snapshot, start.elapsed(), active, snapshots.as_ref());
                if let Some(output) = output { output.summary(report.last_window.as_ref().expect("flushed window"), phase); }
            }
            // Reap completed requests before deciding whether a due arrival hits a cap.
            (index, completion) = poll_fn(|cx| {
                for (index, slot) in slots.iter_mut().enumerate() {
                    if slot.active {
                        if let Poll::Ready(completion) = slot.future.get_pin().poll(cx) { return Poll::Ready((index, completion)); }
                    }
                }
                Poll::Pending
            }), if active > 0 => {
                record_send(&mut slots[index], &mut workers, &mut report);
                slots[index].active = false;
                active -= 1;
                let observation = completion.observation;
                if let Some(output) = output { output.request(slots[index].iteration, phase, &observation, plan.request.body.len()); }
                workers[slots[index].worker].finish(Sample {
                    chain_duration: observation.total, request_duration: observation.request_duration,
                    ttfb: observation.ttfb, drift: None,
                    status: observation.status, error: observation.error.map(cause),
                    bytes_sent: if observation.sent.is_some() { plan.request.body.len() as u64 } else { 0 },
                    bytes_received: observation.bytes_received,
                    connections_opened: observation.connections_opened, connection_reused: observation.connection_reused,
                });
                if observation.sent.is_some() {
                    report.sent_finished += 1;
                }
                if let Some(error) = observation.error {
                    report.failed += 1;
                    report.timed_out += u64::from(error == Failure::Timeout);
                } else { report.responses += 1; }
                pool.release(completion.lease);
            }
            _ = clock.tick(&mut clock_tick), if next.is_some() || active == 0 => {
                let (arrival, late) = schedule.due(Instant::now());
                report.offered += late + u64::from(arrival.is_some());
                report.skipped_late += late;
                if let Some(scheduled) = arrival {
                    if let Some(slot) = slots.iter_mut().find(|s| !s.active) {
                        if let Some(lease) = pool.acquire() {
                            slot.send_state.reset();
                            slot.send_recorded = false;
                            let admitted = Instant::now();
                            let future = execute(Some(Job { lease, endpoint: Arc::clone(&pool.endpoint), template: Arc::clone(&plan.request), scheduled, admitted, send_state: Arc::clone(&slot.send_state) }));
                            // Same execute() future layout for every use; never reallocates.
                            assert!(slot.future.try_set(future).is_ok(), "request future layout changed");
                            slot.active = true;
                            slot.worker = report.admitted as usize % workers.len();
                            slot.iteration = report.admitted;
                            slot.admitted = admitted;
                            workers[slot.worker].start();
                            active += 1;
                            report.admitted += 1;
                            report.peak_in_flight = report.peak_in_flight.max(active);
                        } else { report.skipped_connections += 1; }
                    } else { report.skipped_concurrency += 1; }
                }
            }
        }
    }
    flush(
        Recording {
            slots: &mut slots,
            workers: &mut workers,
            lag: &mut lag,
        },
        &mut interval_metrics,
        &mut report,
        &mut last_snapshot,
        start.elapsed(),
        0,
        snapshots.as_ref(),
    );
    if let Some(output) = output {
        output.summary(report.last_window.as_ref().expect("flushed window"), phase);
        if phase == Phase::Measure {
            output.phase(Phase::Drain);
        }
    }
    // Dropping slots cancels pending body reads; dropping the pool aborts its drivers.
    Ok(report)
}

#[derive(Default)]
struct Lag {
    max: Duration,
    samples: u64,
}

struct Recording<'a> {
    slots: &'a mut [Slot],
    workers: &'a mut [Accumulator],
    lag: &'a mut Lag,
}

fn record_send(slot: &mut Slot, workers: &mut [Accumulator], report: &mut Report) {
    if slot.active && !slot.send_recorded {
        if let Some(drift) = slot.send_state.drift() {
            workers[slot.worker].drift.record(drift);
            report.sent += 1;
            report.max_send_drift = report.max_send_drift.max(drift);
            slot.send_recorded = true;
        }
    }
}

fn flush(
    recording: Recording<'_>,
    interval: &mut Accumulator,
    report: &mut Report,
    last: &mut Duration,
    now: Duration,
    active: usize,
    consumer: Option<&mpsc::Sender<Window>>,
) {
    let Recording {
        slots,
        workers,
        lag,
    } = recording;
    for slot in &mut *slots {
        record_send(slot, workers, report);
    }
    interval.merge_and_reset(workers);
    report.metrics.merge(interval);
    let window = Window {
        from: *last,
        to: now,
        in_flight: active,
        queue_depth: if active == 0 {
            0
        } else {
            slots
                .iter()
                .filter(|slot| slot.active && slot.send_state.drift().is_none())
                .count()
        },
        scheduler_lag: lag.max,
        scheduler_lag_samples: lag.samples,
        metrics: interval.clone(),
    };
    if let Some(consumer) = consumer {
        if consumer.try_send(window.clone()).is_err() {
            report.windows_dropped += 1;
        }
    }
    report.windows += 1;
    report.last_window = Some(window);
    *last = now;
    *lag = Lag::default();
}

fn cause(failure: Failure) -> Cause {
    match failure {
        Failure::Dns => Cause::Dns,
        Failure::Connect => Cause::Connect,
        Failure::Tls => Cause::Tls,
        Failure::Protocol => Cause::Protocol,
        Failure::Send => Cause::Send,
        Failure::Body => Cause::Body,
        Failure::Timeout => Cause::Timeout,
    }
}
