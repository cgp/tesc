//! Fixed-rate load execution independent of the control plane.

mod bundle;
mod http;
mod schedule;
mod wake_clock;

pub use bundle::Plan;
pub use http::Failure;

use std::{
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::ReusableBoxFuture;

use http::{Completion, Job, Pool, execute};
use schedule::Schedule;
use wake_clock::WakeClock;

#[derive(Debug, Default)]
pub struct Report {
    pub offered: u64,
    pub admitted: u64,
    /// Sends among terminal attempts. In-flight cancellations are counted separately.
    pub sent_finished: u64,
    pub responses: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    pub skipped_late: u64,
    pub skipped_concurrency: u64,
    pub skipped_connections: u64,
    pub peak_in_flight: usize,
    pub max_send_drift: Duration,
    pub interrupted: bool,
}

struct Slot {
    active: bool,
    future: ReusableBoxFuture<'static, Completion>,
}

/// The scheduler never writes to stdout/stderr or waits for an output consumer.
pub async fn run(plan: Plan, shutdown: impl Future<Output = ()>) -> Result<Report, String> {
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
            future: ReusableBoxFuture::new(execute(None)),
        });
    }
    let start = Instant::now();
    let mut schedule = Schedule::new(start, plan.rate, plan.duration);
    let clock = WakeClock::start(start, plan.rate, plan.duration)?;
    let mut clock_tick = 0;
    let mut active = 0;
    while Instant::now() < start + plan.duration || schedule.next_deadline().is_some() || active > 0
    {
        let next = schedule.next_deadline();
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                report.interrupted = true;
                report.cancelled = active as u64;
                break;
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
                slots[index].active = false;
                active -= 1;
                let observation = completion.observation;
                if observation.sent.is_some() {
                    report.sent_finished += 1;
                    report.max_send_drift = report.max_send_drift.max(observation.drift);
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
                            let future = execute(Some(Job { lease, endpoint: Arc::clone(&pool.endpoint), template: Arc::clone(&plan.request), scheduled, admitted: Instant::now() }));
                            // Same execute() future layout for every use; never reallocates.
                            assert!(slot.future.try_set(future).is_ok(), "request future layout changed");
                            slot.active = true;
                            active += 1;
                            report.admitted += 1;
                            report.peak_in_flight = report.peak_in_flight.max(active);
                        } else { report.skipped_connections += 1; }
                    } else { report.skipped_concurrency += 1; }
                }
            }
        }
    }
    // Dropping slots cancels pending body reads; dropping the pool aborts its drivers.
    Ok(report)
}
