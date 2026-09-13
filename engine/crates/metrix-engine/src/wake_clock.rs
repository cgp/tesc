//! Coalescing wake-ups from native sleep, without blocking an async worker.

use crate::schedule::Schedule;
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

struct State {
    stopped: AtomicBool,
    sequence: AtomicU64,
    waker: AtomicWaker,
}

pub(crate) struct WakeClock {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl WakeClock {
    pub fn start(start: Instant, rate: f64, duration: Duration) -> Result<Self, String> {
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        });
        let timer = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("metrix-arrivals".into())
            .spawn(move || {
                let end = start + duration;
                let mut schedule = Schedule::new(start, rate, duration);
                while !timer.stopped.load(Ordering::Acquire) {
                    let deadline = schedule.next_deadline().unwrap_or(end);
                    let now = Instant::now();
                    if now < deadline {
                        // Rust's native sleep uses a high-resolution waitable timer on Windows.
                        // Small slices bound cancellation even when the next arrival is far away.
                        std::thread::sleep(
                            deadline.duration_since(now).min(Duration::from_millis(5)),
                        );
                        continue;
                    }
                    schedule.due(now);
                    timer.sequence.fetch_add(1, Ordering::Release);
                    timer.waker.wake();
                    if now >= end {
                        break;
                    }
                }
            })
            .map_err(|_| "cannot create arrival-clock thread")?;
        Ok(Self {
            state,
            thread: Some(thread),
        })
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
