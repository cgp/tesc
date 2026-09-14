//! Absolute phase boundaries; warmup attempts can remain in flight into measure.
use crate::{Plan, schedule::Schedule, wake_clock::WakeClock};
use metrix_metrics::events::Phase;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) struct Timeline {
    pub phase: Phase,
    pub schedule: Option<Schedule>,
    pub clock: Option<WakeClock>,
    pub clock_tick: u64,
    baseline_end: Instant,
    warmup_end: Instant,
    measure_end: Instant,
    settle_end: Option<Instant>,
    warmup: Duration,
    measure: Duration,
    settle: Duration,
    rate: f64,
}

impl Timeline {
    pub fn new(start: Instant, plan: &Plan) -> Result<Self, String> {
        let phase = if !plan.baseline.is_zero() {
            Phase::Baseline
        } else if !plan.warmup.is_zero() {
            Phase::Warmup
        } else {
            Phase::Measure
        };
        let mut timeline = Self {
            phase,
            schedule: None,
            clock: None,
            clock_tick: 0,
            baseline_end: start + plan.baseline,
            warmup_end: start + plan.baseline + plan.warmup,
            measure_end: start + plan.baseline + plan.warmup + plan.duration,
            settle_end: None,
            warmup: plan.warmup,
            measure: plan.duration,
            settle: plan.settle,
            rate: plan.rate,
        };
        timeline.enter(phase, start)?;
        Ok(timeline)
    }

    pub(crate) fn stop(&mut self, now: Instant, settle: Duration) {
        self.schedule = None;
        self.clock = None;
        self.measure_end = now;
        self.settle = settle;
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Baseline => Some(self.baseline_end),
            Phase::Warmup => Some(self.warmup_end),
            Phase::Measure => Some(self.measure_end),
            Phase::Drain => None,
            Phase::Settle => self.settle_end,
        }
    }

    pub fn ready(&self, now: Instant, active: usize) -> bool {
        if self.phase == Phase::Drain {
            active == 0
        } else {
            self.deadline().is_some_and(|deadline| now >= deadline)
        }
    }

    pub fn advance(&mut self, now: Instant) -> Result<bool, String> {
        let next = match self.phase {
            Phase::Baseline if !self.warmup.is_zero() => Phase::Warmup,
            Phase::Baseline | Phase::Warmup => Phase::Measure,
            Phase::Measure => Phase::Drain,
            Phase::Drain if !self.settle.is_zero() => Phase::Settle,
            Phase::Drain | Phase::Settle => return Ok(false),
        };
        self.enter(next, now)?;
        Ok(true)
    }

    fn enter(&mut self, phase: Phase, now: Instant) -> Result<(), String> {
        self.clock = None;
        self.schedule = None;
        self.clock_tick = 0;
        self.phase = phase;
        let traffic = match phase {
            Phase::Warmup => Some((self.baseline_end, self.warmup)),
            Phase::Measure => Some((self.warmup_end, self.measure)),
            _ => None,
        };
        if let Some((start, duration)) = traffic {
            self.schedule = Some(Schedule::new(start, self.rate, duration));
            self.clock = Some(WakeClock::start(start, self.rate, duration)?);
        }
        if phase == Phase::Settle {
            self.settle_end = Some(now + self.settle);
        }
        Ok(())
    }
}
