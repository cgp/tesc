//! Phase execution with separate accumulators for warmup-admitted attempts.
use crate::{Lag, Output, Plan, Recording, Report, Slot, cause, flush, record_send};
use crate::{
    http::{Job, Pool, SendState, execute},
    timeline::Timeline,
};
use metrix_metrics::{
    aggregation::{Accumulator, Sample, Window},
    events::Phase,
};
use std::{
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::ReusableBoxFuture;

pub(crate) async fn run(
    plan: Plan,
    shutdown: impl Future<Output = ()>,
    snapshots: Option<mpsc::Sender<Window>>,
    output: Option<&Output>,
) -> Result<Report, String> {
    tokio::pin!(shutdown);
    let mut report = Report::default();
    let pool = tokio::select! {
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
            phase: Phase::Measure,
            send_state: Arc::new(SendState::default()),
            send_recorded: false,
            future: ReusableBoxFuture::new(execute(None)),
        });
    }
    let mut workers: Vec<_> = (0..plan.worker_threads.min(plan.concurrency))
        .map(|_| Accumulator::default())
        .collect();
    let mut warmup_workers: Vec<_> = (0..workers.len()).map(|_| Accumulator::default()).collect();
    let mut interval_metrics = Accumulator::default();
    let mut warmup_interval = Accumulator::default();
    let mut lag = Lag::default();
    let start = Instant::now();
    let measure_from_ms = output.map(|o| {
        o.elapsed().saturating_add(
            (plan.baseline + plan.warmup)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        )
    });
    let mut timeline = Timeline::new(start, &plan)?;
    if let Some(output) = output {
        output.phase(timeline.phase);
    }
    let mut last_snapshot = Duration::ZERO;
    let mut snapshot_tick = tokio::time::interval_at(
        start + Duration::from_millis(250),
        Duration::from_millis(250),
    );
    snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active = 0;
    let mut pool = Some(pool);
    let mut initial_connection_counted = false;
    loop {
        let phase = timeline.phase;
        if !initial_connection_counted && matches!(phase, Phase::Warmup | Phase::Measure) {
            if phase == Phase::Warmup {
                warmup_workers[0].counters.connections_opened = 1;
            } else {
                workers[0].counters.connections_opened = 1;
            }
            initial_connection_counted = true;
        }
        if timeline.ready(Instant::now(), active) {
            if let Some(schedule) = &mut timeline.schedule {
                let (arrival, late) = schedule.due(Instant::now());
                debug_assert!(arrival.is_none());
                report.offered += late;
                report.skipped_late += late;
            }
            flush(
                Recording {
                    slots: &mut slots,
                    workers: &mut workers,
                    warmup_workers: &mut warmup_workers,
                    warmup_interval: &mut warmup_interval,
                    lag: &mut lag,
                    phase,
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
            }
            if !timeline.advance(Instant::now())? {
                break;
            }
            if timeline.phase == Phase::Settle {
                drop(pool.take());
            }
            if let Some(output) = output {
                output.phase(timeline.phase);
            }
            continue;
        }
        let deadline = timeline.deadline();
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                report.interrupted = true;
                report.cancelled = active as u64;
                for slot in &slots { if slot.active {
                    if slot.phase == Phase::Warmup { warmup_workers[slot.worker].cancel(); } else { workers[slot.worker].cancel(); }
                    if let Some(output) = output { output.cancel(slot.iteration, slot.phase, slot.admitted.elapsed()); }
                } }
                flush(Recording { slots: &mut slots, workers: &mut workers, warmup_workers: &mut warmup_workers, warmup_interval: &mut warmup_interval, lag: &mut lag, phase },
                    &mut interval_metrics, &mut report, &mut last_snapshot, start.elapsed(), 0, snapshots.as_ref());
                if let Some(output) = output { output.summary(report.last_window.as_ref().expect("flushed window"), phase); }
                break;
            }
            _ = async { if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } } => {}
            deadline = snapshot_tick.tick() => {
                let late = Instant::now().saturating_duration_since(deadline);
                lag.max = lag.max.max(late);
                lag.samples += 1;
                report.max_scheduler_lag = report.max_scheduler_lag.max(late);
                report.scheduler_lag_samples += 1;
                flush(Recording { slots: &mut slots, workers: &mut workers, warmup_workers: &mut warmup_workers, warmup_interval: &mut warmup_interval, lag: &mut lag, phase },
                    &mut interval_metrics, &mut report, &mut last_snapshot, start.elapsed(), active, snapshots.as_ref());
                if let Some(output) = output { output.summary(report.last_window.as_ref().expect("flushed window"), phase); }
            }
            (index, completion) = poll_fn(|cx| {
                for (index, slot) in slots.iter_mut().enumerate() {
                    if slot.active {
                        if let Poll::Ready(completion) = slot.future.get_pin().poll(cx) { return Poll::Ready((index, completion)); }
                    }
                }
                Poll::Pending
            }), if active > 0 => {
                record_send(&mut slots[index], &mut workers, &mut warmup_workers, &mut report);
                slots[index].active = false;
                active -= 1;
                let observation = completion.observation;
                let admitted_phase = slots[index].phase;
                if let Some(output) = output { output.request(slots[index].iteration, admitted_phase, &observation, plan.request.body.len()); }
                let worker = if admitted_phase == Phase::Warmup { &mut warmup_workers[slots[index].worker] } else { &mut workers[slots[index].worker] };
                worker.finish(Sample {
                    chain_duration: observation.total, request_duration: observation.request_duration,
                    ttfb: observation.ttfb, drift: None,
                    status: observation.status, error: observation.error.map(cause),
                    bytes_sent: if observation.sent.is_some() { plan.request.body.len() as u64 } else { 0 },
                    bytes_received: observation.bytes_received,
                    connections_opened: observation.connections_opened, connection_reused: observation.connection_reused,
                });
                if observation.sent.is_some() { report.sent_finished += 1; }
                if let Some(error) = observation.error {
                    report.failed += 1;
                    report.timed_out += u64::from(error == crate::Failure::Timeout);
                } else { report.responses += 1; }
                pool.as_mut().expect("pool while requests are active").release(completion.lease);
            }
            _ = async { timeline.clock.as_ref().expect("traffic clock").tick(&mut timeline.clock_tick).await; }, if timeline.clock.is_some() => {
                let (arrival, late) = timeline.schedule.as_mut().expect("traffic schedule").due(Instant::now());
                report.offered += late + u64::from(arrival.is_some());
                report.skipped_late += late;
                if let Some(scheduled) = arrival {
                    if let Some(slot) = slots.iter_mut().find(|s| !s.active) {
                        if let Some(lease) = pool.as_mut().expect("traffic pool").acquire() {
                            slot.send_state.reset();
                            slot.send_recorded = false;
                            let admitted = Instant::now();
                            let endpoint = Arc::clone(&pool.as_ref().expect("traffic pool").endpoint);
                            let future = execute(Some(Job { lease, endpoint, template: Arc::clone(&plan.request), scheduled, admitted, send_state: Arc::clone(&slot.send_state) }));
                            assert!(slot.future.try_set(future).is_ok(), "request future layout changed");
                            slot.active = true;
                            slot.worker = report.admitted as usize % workers.len();
                            slot.iteration = report.admitted;
                            slot.admitted = admitted;
                            slot.phase = phase;
                            if phase == Phase::Warmup { warmup_workers[slot.worker].start(); } else { workers[slot.worker].start(); }
                            active += 1;
                            report.admitted += 1;
                            report.peak_in_flight = report.peak_in_flight.max(active);
                        } else { report.skipped_connections += 1; }
                    } else { report.skipped_concurrency += 1; }
                }
            }
        }
    }
    if let Some(output) = output {
        output.percentiles(
            &report.metrics,
            measure_from_ms.expect("output clock"),
            report.interrupted,
        );
    }
    Ok(report)
}
