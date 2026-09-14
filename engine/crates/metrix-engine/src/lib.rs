//! Fixed-rate load execution independent of the control plane.

mod assertions;
mod bundle;
mod calibration;
mod calls;
mod chain;
mod detectors;
mod extract;
pub use detectors::{DetectorConfig, Diagnostics, Health as PhaseHealth};
mod execution;
mod http;
mod output;
mod schedule;
mod template;
mod timeline;
mod wake_clock;

pub use bundle::Plan;
pub use calibration::{MachineProfile, calibrate, write_profile};
pub use http::Failure;
use metrix_metrics::events::Phase;
pub use output::{Output, OutputReport};

use metrix_metrics::aggregation::{Accumulator, Cause, Window};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::ReusableBoxFuture;

use http::SendState;

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
    /// Warmup-admitted attempts, including completions after the warmup boundary.
    pub warmup_metrics: Accumulator,
    pub last_window: Option<Window>,
    pub windows: u64,
    pub windows_dropped: u64,
    /// Iterations that stopped before their last step. Counted apart from failed
    /// requests so one upstream 500 does not inflate the error rate three times
    /// over (design-engine §5).
    pub chains_aborted: u64,
    pub diagnostics: Diagnostics,
}

struct Slot {
    active: bool,
    worker: usize,
    iteration: u64,
    admitted: Instant,
    phase: Phase,
    send_state: Arc<SendState>,
    send_recorded: bool,
    /// Which chain this slot is running, so a cancellation can be counted against
    /// the chain it belonged to rather than against the run in general.
    chain: &'static str,
    future: ReusableBoxFuture<'static, chain::Completion>,
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
    // Keep the phase accumulators off the Windows CLI's small main-thread stack.
    Box::pin(execution::run(plan, shutdown, snapshots, output)).await
}

#[derive(Default)]
struct Lag {
    max: Duration,
    samples: u64,
}

struct Recording<'a> {
    slots: &'a mut [Slot],
    workers: &'a mut [Accumulator],
    warmup_workers: &'a mut [Accumulator],
    warmup_interval: &'a mut Accumulator,
    phase: Phase,
    lag: &'a mut Lag,
}

fn record_send(
    slot: &mut Slot,
    workers: &mut [Accumulator],
    warmup_workers: &mut [Accumulator],
    report: &mut Report,
) {
    if slot.active && !slot.send_recorded {
        if let Some(drift) = slot.send_state.drift() {
            let worker = if slot.phase == Phase::Warmup {
                &mut warmup_workers[slot.worker]
            } else {
                &mut workers[slot.worker]
            };
            worker.drift.record(drift);
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
        warmup_workers,
        warmup_interval,
        phase,
        lag,
    } = recording;
    for slot in &mut *slots {
        record_send(slot, workers, warmup_workers, report);
    }
    interval.merge_and_reset(workers);
    report.metrics.merge(interval);
    warmup_interval.merge_and_reset(warmup_workers);
    report.warmup_metrics.merge(warmup_interval);
    report
        .diagnostics
        .sync(&report.metrics, &report.warmup_metrics);
    let warmup_active = if active == 0 {
        0
    } else {
        slots
            .iter()
            .filter(|s| s.active && s.phase == Phase::Warmup)
            .count()
    };
    let warmup_queue = if active == 0 {
        0
    } else {
        slots
            .iter()
            .filter(|s| s.active && s.phase == Phase::Warmup && s.send_state.drift().is_none())
            .count()
    };
    let carry = phase != Phase::Warmup
        && (warmup_active > 0
            || warmup_interval.counters.completed > 0
            || warmup_interval.counters.failed > 0
            || warmup_interval.counters.cancelled > 0
            || warmup_interval.drift.count() > 0
            || warmup_interval.drift.overflow > 0);
    let window = Window {
        phase,
        warmup_metrics: carry.then(|| Box::new(warmup_interval.clone())),
        warmup_in_flight: warmup_active,
        warmup_queue_depth: warmup_queue,
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
        metrics: if phase == Phase::Warmup {
            warmup_interval.clone()
        } else {
            interval.clone()
        },
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
