//! Coalescing wake-ups from native sleep, without blocking an async worker.

use crate::schedule::Schedule;
use metrix_metrics::aggregation::ArrivalTiming;

use atomic_waker::AtomicWaker;
use std::{
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::Poll,
    thread::JoinHandle,
    time::Duration,
};
use tokio::time::Instant;

const TELEMETRY_CAPACITY: usize = 1024;

struct WakeSample {
    sequence: u64,
    deadline: Instant,
    woke: Instant,
    skipped: u64,
}

// Single producer and single consumer. Slots are allocated once; release/acquire
// head/tail publication prevents slot reuse while a reader is copying its fields.
#[derive(Default)]
struct SampleSlot {
    sequence: AtomicU64,
    deadline: AtomicU64,
    woke: AtomicU64,
    skipped: AtomicU64,
}
struct Samples {
    slots: Box<[SampleSlot]>,
    head: AtomicU64,
    tail: AtomicU64,
}
impl Samples {
    fn new() -> Self {
        Self {
            slots: (0..TELEMETRY_CAPACITY)
                .map(|_| SampleSlot::default())
                .collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }
    fn push(&self, origin: Instant, sample: WakeSample) {
        let head = self.head.load(Ordering::Relaxed);
        if head - self.tail.load(Ordering::Acquire) >= TELEMETRY_CAPACITY as u64 {
            return;
        }
        let (Ok(deadline), Ok(woke)) = (
            u64::try_from(sample.deadline.duration_since(origin).as_nanos()),
            u64::try_from(sample.woke.duration_since(origin).as_nanos()),
        ) else {
            return;
        };
        let slot = &self.slots[head as usize % TELEMETRY_CAPACITY];
        slot.sequence.store(sample.sequence, Ordering::Relaxed);
        slot.deadline.store(deadline, Ordering::Relaxed);
        slot.woke.store(woke, Ordering::Relaxed);
        slot.skipped.store(sample.skipped, Ordering::Relaxed);
        self.head.store(head + 1, Ordering::Release);
    }
    fn pop(&self, origin: Instant) -> Option<WakeSample> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let slot = &self.slots[tail as usize % TELEMETRY_CAPACITY];
        let sample = WakeSample {
            sequence: slot.sequence.load(Ordering::Relaxed),
            deadline: origin + Duration::from_nanos(slot.deadline.load(Ordering::Relaxed)),
            woke: origin + Duration::from_nanos(slot.woke.load(Ordering::Relaxed)),
            skipped: slot.skipped.load(Ordering::Relaxed),
        };
        self.tail.store(tail + 1, Ordering::Release);
        Some(sample)
    }
}

pub(crate) struct Checkpoint {
    sequence: u64,
    dispatch: Instant,
}
struct State {
    samples: Samples,
    stopped: AtomicBool,
    sequence: AtomicU64,
    waker: AtomicWaker,
}

pub(crate) struct WakeClock {
    origin: Instant,
    pending: Option<WakeSample>,
    observed: u64,
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl WakeClock {
    pub fn start(start: Instant, rate: f64, duration: Duration) -> Result<Self, String> {
        let state = Arc::new(State {
            samples: Samples::new(),
            stopped: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        });
        let timer = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("metrix-arrivals".into())
            .spawn(move || {
                let end = start + duration;
                let spin_margin = Duration::from_secs_f64((0.25 / rate).min(0.001));
                let mut schedule = Schedule::new(start, rate, duration);
                while !timer.stopped.load(Ordering::Acquire) {
                    let deadline = schedule.next_deadline().unwrap_or(end);
                    let now = Instant::now();
                    if now < deadline {
                        // Rust's native sleep uses a high-resolution waitable timer on Windows.
                        // Small slices bound cancellation even when the next arrival is far away.
                        let remaining = deadline.duration_since(now);
                        if remaining > spin_margin {
                            std::thread::sleep(
                                (remaining - spin_margin).min(Duration::from_millis(5)),
                            );
                        } else {
                            std::hint::spin_loop();
                        }
                        continue;
                    }
                    let (_, skipped) = schedule.due(now);
                    let sequence = timer.sequence.load(Ordering::Relaxed) + 1;
                    // Publish timestamps before the sequence. A full diagnostic queue
                    // drops evidence, never a wake or an offered arrival.
                    timer.samples.push(
                        start,
                        WakeSample {
                            sequence,
                            deadline,
                            woke: now,
                            skipped,
                        },
                    );
                    timer.sequence.store(sequence, Ordering::Release);
                    timer.waker.wake();
                    if now >= end {
                        break;
                    }
                }
            })
            .map_err(|_| "cannot create arrival-clock thread")?;
        Ok(Self {
            origin: start,
            pending: None,
            observed: 0,
            state,
            thread: Some(thread),
        })
    }

    pub fn checkpoint(&self) -> Checkpoint {
        let sequence = self.state.sequence.load(Ordering::Acquire);
        Checkpoint {
            sequence,
            dispatch: Instant::now(),
        }
    }

    pub fn record(&mut self, checkpoint: Checkpoint, timing: &mut ArrivalTiming) {
        let wakes = checkpoint.sequence - self.observed;
        let mut retained = 0;
        // A bounded drain cannot chase a concurrently publishing producer forever.
        for _ in 0..=TELEMETRY_CAPACITY {
            let sample = match self
                .pending
                .take()
                .or_else(|| self.state.samples.pop(self.origin))
            {
                Some(sample) => sample,
                None => break,
            };
            if sample.sequence > checkpoint.sequence {
                self.pending = Some(sample);
                break;
            }
            timing
                .timer_wake_lateness
                .record(sample.woke.saturating_duration_since(sample.deadline));
            timing
                .wake_to_dispatch
                .record(checkpoint.dispatch.saturating_duration_since(sample.woke));
            timing.timer_skipped_arrivals += sample.skipped;
            retained += 1;
        }
        timing.timer_wakes += wakes;
        timing.coalesced_wakes += wakes.saturating_sub(1);
        timing.telemetry_dropped += wakes - retained;
        self.observed = checkpoint.sequence;
    }

    pub async fn tick(&self, last: &mut u64) {
        poll_fn(|cx| {
            self.state.waker.register(cx.waker());
            let current = self.state.sequence.load(Ordering::Acquire);
            if current != *last {
                *last = current;
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

impl Drop for WakeClock {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn clock() -> WakeClock {
        WakeClock {
            origin: Instant::now(),
            pending: None,
            observed: 0,
            state: Arc::new(State {
                samples: Samples::new(),
                stopped: AtomicBool::new(false),
                sequence: AtomicU64::new(0),
                waker: AtomicWaker::new(),
            }),
            thread: None,
        }
    }
    fn sample(origin: Instant, sequence: u64) -> WakeSample {
        WakeSample {
            sequence,
            deadline: origin + Duration::from_micros(sequence),
            woke: origin + Duration::from_micros(sequence + 2),
            skipped: sequence % 7,
        }
    }
    #[test]
    fn coalesced_wakes_separate_timer_lateness_from_dispatch_delay() {
        let mut clock = clock();
        clock
            .state
            .samples
            .push(clock.origin, sample(clock.origin, 1));
        clock
            .state
            .samples
            .push(clock.origin, sample(clock.origin, 2));
        let mut timing = ArrivalTiming::default();
        clock.record(
            Checkpoint {
                sequence: 2,
                dispatch: clock.origin + Duration::from_micros(30),
            },
            &mut timing,
        );
        assert_eq!(timing.timer_wake_lateness.count(), 2);
        assert_eq!(timing.timer_wake_lateness.max_us(), Some(2));
        assert_eq!(timing.wake_to_dispatch.max_us(), Some(27));
        assert_eq!(timing.timer_wakes, 2);
        assert_eq!(timing.coalesced_wakes, 1);
        assert_eq!(timing.telemetry_dropped, 0);
        assert_eq!(timing.timer_skipped_arrivals, 3);
        // Diagnostic reads do not advance the arrival schedule or replay missed arrivals.
        let mut schedule = Schedule::new(clock.origin, 1000.0, Duration::from_secs(1));
        let dispatch = clock.origin + Duration::from_millis(3);
        assert_eq!(schedule.due(dispatch), (Some(dispatch), 3));
        timing.dispatch(3, false);
        assert_eq!(schedule.due(dispatch), (None, 0));
        assert_eq!(timing.dispatches_with_skips, 1);
        assert_eq!(timing.max_skipped_per_dispatch, 3);
    }
    #[test]
    fn full_diagnostic_ring_drops_samples_without_losing_notifications() {
        let mut clock = clock();
        for n in 1..=TELEMETRY_CAPACITY as u64 + 7 {
            clock
                .state
                .samples
                .push(clock.origin, sample(clock.origin, n));
        }
        let mut timing = ArrivalTiming::default();
        clock.record(
            Checkpoint {
                sequence: TELEMETRY_CAPACITY as u64 + 7,
                dispatch: clock.origin + Duration::from_secs(1),
            },
            &mut timing,
        );
        assert_eq!(timing.timer_wakes, TELEMETRY_CAPACITY as u64 + 7);
        assert_eq!(timing.telemetry_dropped, 7);
        assert_eq!(timing.wake_to_dispatch.count(), TELEMETRY_CAPACITY as u64);
        let next = TELEMETRY_CAPACITY as u64 + 8;
        clock
            .state
            .samples
            .push(clock.origin, sample(clock.origin, next));
        clock.record(
            Checkpoint {
                sequence: next,
                dispatch: clock.origin + Duration::from_secs(2),
            },
            &mut timing,
        );
        assert_eq!(timing.telemetry_dropped, 7);
        assert_eq!(
            timing.timer_wakes,
            timing.wake_to_dispatch.count() + timing.telemetry_dropped
        );
    }
    #[test]
    fn an_unpublished_future_sample_is_retained_across_a_full_ring() {
        let mut clock = clock();
        clock
            .state
            .samples
            .push(clock.origin, sample(clock.origin, 1));
        clock
            .state
            .samples
            .push(clock.origin, sample(clock.origin, 2));
        let mut timing = ArrivalTiming::default();
        clock.record(
            Checkpoint {
                sequence: 1,
                dispatch: clock.origin + Duration::from_micros(10),
            },
            &mut timing,
        );
        for n in 3..=TELEMETRY_CAPACITY as u64 + 2 {
            clock
                .state
                .samples
                .push(clock.origin, sample(clock.origin, n));
        }
        clock.record(
            Checkpoint {
                sequence: TELEMETRY_CAPACITY as u64 + 2,
                dispatch: clock.origin + Duration::from_secs(1),
            },
            &mut timing,
        );
        assert_eq!(timing.telemetry_dropped, 0);
        assert_eq!(
            timing.wake_to_dispatch.count(),
            TELEMETRY_CAPACITY as u64 + 2
        );
    }
    #[test]
    fn concurrent_ring_reuse_preserves_timestamp_and_sequence_pairs() {
        let origin = Instant::now();
        let samples = Arc::new(Samples::new());
        let done = Arc::new(AtomicBool::new(false));
        let writer = Arc::clone(&samples);
        let finished = Arc::clone(&done);
        let task = std::thread::spawn(move || {
            for n in 1..=100_000 {
                writer.push(origin, sample(origin, n));
            }
            finished.store(true, Ordering::Release);
        });
        let mut previous = 0;
        loop {
            if let Some(sample) = samples.pop(origin) {
                assert!(sample.sequence > previous);
                assert_eq!(
                    sample.deadline.duration_since(origin).as_micros(),
                    sample.sequence as u128
                );
                assert_eq!(
                    sample.woke.duration_since(sample.deadline),
                    Duration::from_micros(2)
                );
                assert_eq!(sample.skipped, sample.sequence % 7);
                previous = sample.sequence;
            } else if done.load(Ordering::Acquire) {
                break;
            } else {
                std::thread::yield_now();
            }
        }
        task.join().unwrap();
        assert!(previous > 0);
    }
}
