//! Absolute arrival deadlines, independent of response completion.

use std::time::Duration;
use tokio::time::Instant;

pub(crate) struct Schedule {
    start: Instant,
    duration: Duration,
    rate: f64,
    next: u64,
    total: u64,
}

impl Schedule {
    pub fn validate(rate: f64, duration: Duration) -> Result<(), String> {
        if !rate.is_finite() || rate <= 0.0 || rate > 1e9 {
            return Err("mix.json#/load/rate: must be finite, positive, and at most one arrival per nanosecond".into());
        }
        if duration.is_zero()
            || Instant::now().checked_add(duration).is_none()
            || duration.as_secs_f64() * rate > (1_u64 << 53) as f64
        {
            return Err(
                "mix.json#/load: duration must be positive and the schedule representable".into(),
            );
        }
        Ok(())
    }

    pub fn new(start: Instant, rate: f64, duration: Duration) -> Self {
        let mut schedule = Self {
            start,
            rate,
            duration,
            next: 0,
            total: (rate * duration.as_secs_f64()).ceil() as u64,
        };
        // Floating point may put an integral product just above the boundary.
        while schedule.total > 0 && schedule.offset(schedule.total - 1) >= duration {
            schedule.total -= 1;
        }
        schedule
    }

    fn offset(&self, index: u64) -> Duration {
        Duration::from_secs_f64(index as f64 / self.rate)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        (self.next < self.total).then(|| self.start + self.offset(self.next))
    }

    /// Returns at most one arrival and the number skipped. Never replays a backlog.
    pub fn due(&mut self, now: Instant) -> (Option<Instant>, u64) {
        if self.next >= self.total || now < self.start + self.offset(self.next) {
            return (None, 0);
        }
        let elapsed = now.duration_since(self.start);
        if elapsed >= self.duration {
            let missed = self.total - self.next;
            self.next = self.total;
            return (None, missed);
        }
        let mut latest = ((elapsed.as_secs_f64() * self.rate).floor() as u64)
            .min(self.total - 1)
            .max(self.next);
        while latest > self.next && self.offset(latest) > elapsed {
            latest -= 1;
        }
        while latest + 1 < self.total && self.offset(latest + 1) <= elapsed {
            latest += 1;
        }
        let skipped = latest - self.next;
        self.next = latest + 1;
        (Some(self.start + self.offset(latest)), skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_rates_use_absolute_deadlines_without_rounding_accumulation() {
        for (rate, seconds, expected) in [
            (75.0, 30, 2250),
            (2.5, 3, 8),
            (1.0 / 3.0, 30, 10),
            (0.01, 1, 1),
        ] {
            let start = Instant::now();
            let mut clock = Schedule::new(start, rate, Duration::from_secs(seconds));
            let mut count = 0;
            while let Some(deadline) = clock.next_deadline() {
                assert!(deadline < start + Duration::from_secs(seconds));
                assert_eq!(
                    deadline.duration_since(start),
                    Duration::from_secs_f64(f64::from(count) / rate)
                );
                assert_eq!(clock.due(deadline), (Some(deadline), 0));
                count += 1;
            }
            assert_eq!(count, expected);
        }
    }

    #[test]
    fn late_wakes_drop_old_arrivals_without_replaying_a_burst() {
        let start = Instant::now();
        let mut clock = Schedule::new(start, 100.0, Duration::from_secs(1));
        assert_eq!(clock.due(start), (Some(start), 0));
        let late = start + Duration::from_millis(257);
        assert_eq!(
            clock.due(late),
            (Some(start + Duration::from_millis(250)), 24)
        );
        assert_eq!(clock.due(late), (None, 0));
        assert_eq!(
            clock.next_deadline(),
            Some(start + Duration::from_millis(260))
        );
        assert_eq!(clock.due(start + Duration::from_secs(1)), (None, 74));
        assert_eq!(clock.next_deadline(), None);
    }

    #[test]
    fn rejects_invalid_or_unrepresentable_schedules() {
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e10] {
            assert!(Schedule::validate(rate, Duration::from_secs(1)).is_err());
        }
        assert!(Schedule::validate(75.0, Duration::ZERO).is_err());
        assert!(Schedule::validate(75.0, Duration::MAX).is_err());
    }
}
