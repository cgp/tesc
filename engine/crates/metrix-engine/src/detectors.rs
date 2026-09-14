//! Scalar observation on the scheduler; annotation construction on writer threads.
use metrix_metrics::aggregation::Accumulator;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct DetectorConfig {
    pub rate_tolerance_pct: u8,
    pub drift_threshold: Duration,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Seen {
    cap: Option<metrix_metrics::Severity>,
    rate: bool,
    drift: bool,
}

pub(crate) fn annotations(
    h: Health,
    d: Diagnostics,
    phase: metrix_metrics::Phase,
    clock: (u64, u64),
    closing: bool,
    partial: bool,
    seen: &mut Seen,
) -> Vec<metrix_metrics::events::Annotation> {
    use metrix_metrics::{Severity, events::Annotation};
    let (base, t_ms) = clock;
    let mut result = Vec::new();
    if h.observed.is_zero() {
        return result;
    }
    let ms = |v: Duration| v.as_millis().min(u128::from(u64::MAX)) as u64;
    let end = base.saturating_add(ms(h.from + h.observed)).min(t_ms);
    let from = base.saturating_add(ms(h.from)).min(end);
    let mut push = |code: &str, severity, message: &str, first, last, detail| {
        result.push(Annotation {
            t_ms,
            target_id: None,
            code: code.into(),
            severity,
            phase: Some(phase),
            from_ms: first,
            to_ms: Some(last),
            message: message.into(),
            detail: Some(detail),
        })
    };
    let denominator = if partial { h.observed } else { h.end - h.from };
    if !h.cap_duration.is_zero() {
        let severity = if phase == metrix_metrics::Phase::Measure
            && h.cap_duration.saturating_mul(4) > denominator
        {
            Severity::Invalid
        } else {
            Severity::Warn
        };
        if seen.cap != Some(severity) || closing {
            push(
                "concurrency_cap_reached",
                severity,
                "In-flight requests occupied the concurrency cap; this can limit offered traffic independently of target capacity.",
                base.saturating_add(ms(h.first_cap.unwrap())),
                base.saturating_add(ms(h.last_cap)).min(t_ms),
                serde_json::json!({"cap": d.cap, "duration_ms": ms(h.cap_duration), "window_pct": h.cap_duration.as_secs_f64() / denominator.as_secs_f64() * 100.0, "peak_in_flight": h.peak_in_flight, "partial": partial}),
            );
            seen.cap = Some(severity);
        }
    }
    let grace = u64::from(!closing);
    let missing = h.offered.saturating_sub(h.sends.saturating_add(grace));
    if (closing || h.observed >= Duration::from_secs(1))
        && h.offered > 0
        && missing as f64 / h.offered as f64 * 100.0 > f64::from(d.config.rate_tolerance_pct)
        && (!seen.rate || closing)
    {
        push(
            "rate_not_achieved",
            Severity::Warn,
            "Observed phase-admitted sends fell below discrete offered arrivals beyond the configured tolerance.",
            from,
            end,
            serde_json::json!({"offered": h.offered, "sends": h.sends, "tolerance_pct": d.config.rate_tolerance_pct, "pending_grace": grace, "skipped_late": h.skipped_late, "skipped_concurrency": h.skipped_concurrency, "skipped_connections": h.skipped_connections, "window_ms": ms(h.observed), "partial": partial}),
        );
        seen.rate = true;
    }
    if h.drift_samples > 0 && h.max_drift > d.config.drift_threshold && (!seen.drift || closing) {
        push(
            "send_schedule_drift",
            Severity::Invalid,
            "Observed sends exceeded the configured schedule-drift threshold; generator delay is included in corrected latency.",
            from,
            end,
            serde_json::json!({"max_drift_ms": h.max_drift.as_secs_f64() * 1000.0, "threshold_ms": d.config.drift_threshold.as_secs_f64() * 1000.0, "sample_count": h.drift_samples, "partial": partial}),
        );
        seen.drift = true;
    }
    result
}
impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            rate_tolerance_pct: 2,
            drift_threshold: Duration::from_millis(5),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Health {
    pub from: Duration,
    pub end: Duration,
    pub observed: Duration,
    pub cap_duration: Duration,
    pub first_cap: Option<Duration>,
    pub last_cap: Duration,
    pub peak_in_flight: usize,
    pub offered: u64,
    pub skipped_late: u64,
    pub skipped_concurrency: u64,
    pub skipped_connections: u64,
    pub sends: u64,
    pub drift_samples: u64,
    pub max_drift: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Diagnostics {
    pub config: DetectorConfig,
    pub cap: usize,
    pub warmup: Health,
    pub measure: Health,
    last: Duration,
    previous_active: usize,
}
impl Diagnostics {
    pub(crate) fn new(
        config: DetectorConfig,
        cap: usize,
        baseline: Duration,
        warmup: Duration,
        measure: Duration,
    ) -> Self {
        Self {
            config,
            cap,
            warmup: Health {
                from: baseline,
                end: baseline + warmup,
                ..Health::default()
            },
            measure: Health {
                from: baseline + warmup,
                end: baseline + warmup + measure,
                ..Health::default()
            },
            ..Self::default()
        }
    }
    pub(crate) fn observe(&mut self, now: Duration, active: usize) {
        for h in [&mut self.warmup, &mut self.measure] {
            let from = self.last.max(h.from);
            let to = now.min(h.end);
            h.observed = to.saturating_sub(h.from);
            if to > from {
                h.peak_in_flight = h.peak_in_flight.max(self.previous_active);
                if self.previous_active == self.cap {
                    h.cap_duration += to - from;
                    h.first_cap.get_or_insert(from);
                    h.last_cap = to;
                }
            }
            if now >= h.from && now < h.end {
                h.peak_in_flight = h.peak_in_flight.max(active);
            }
        }
        self.last = now;
        self.previous_active = active;
    }
    pub(crate) fn phase_mut(&mut self, phase: metrix_metrics::Phase) -> &mut Health {
        match phase {
            metrix_metrics::Phase::Warmup => &mut self.warmup,
            metrix_metrics::Phase::Measure => &mut self.measure,
            _ => unreachable!("offered traffic phase"),
        }
    }
    pub(crate) fn sync(&mut self, measured: &Accumulator, warmup: &Accumulator) {
        for (h, metrics) in [(&mut self.measure, measured), (&mut self.warmup, warmup)] {
            h.sends = metrics.drift.count() + metrics.drift.overflow;
            h.drift_samples = h.sends;
            h.max_drift = Duration::from_micros(metrics.drift.max_us().unwrap_or(0));
            if metrics.drift.overflow > 0 {
                h.max_drift = Duration::MAX;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn occupancy_is_clipped_to_phases_and_includes_carried_warmup_slots() {
        let s = Duration::from_secs;
        let mut d = Diagnostics::new(DetectorConfig::default(), 2, s(1), s(2), s(4));
        d.observe(s(0), 2);
        d.observe(s(2), 2);
        d.observe(s(4), 1);
        d.observe(s(10), 0);
        assert_eq!(d.warmup.cap_duration, s(2));
        assert_eq!(d.measure.cap_duration, s(1));
        assert_eq!(d.measure.observed, s(4));
        assert_eq!(d.measure.peak_in_flight, 2);
        assert_eq!(d.measure.first_cap, Some(s(3)));
        assert_eq!(d.measure.last_cap, s(4));
    }

    #[test]
    fn strict_thresholds_grace_deduplication_and_warmup_severity() {
        use metrix_metrics::{Phase, Severity};
        let s = Duration::from_secs;
        let mut d = Diagnostics::new(DetectorConfig::default(), 1, s(0), s(1), s(4));
        let mut h = Health {
            from: s(1),
            end: s(5),
            observed: s(2),
            cap_duration: s(1),
            first_cap: Some(s(1)),
            last_cap: s(2),
            offered: 100,
            sends: 98,
            drift_samples: 98,
            max_drift: Duration::from_millis(5),
            ..Health::default()
        };
        let mut seen = Seen::default();
        let a = annotations(h, d, Phase::Measure, (100, 2100), false, false, &mut seen);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].severity, Severity::Warn); // exactly 25%, no escalation
        assert!(annotations(h, d, Phase::Measure, (100, 2100), false, false, &mut seen).is_empty());
        h.cap_duration += Duration::from_millis(1);
        h.sends = 96; // live grace leaves a strict 3% shortfall
        h.max_drift += Duration::from_micros(1);
        let a = annotations(h, d, Phase::Measure, (100, 2100), false, false, &mut seen);
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].severity, Severity::Invalid);
        assert_eq!(a[1].code, "rate_not_achieved");
        assert_eq!(a[2].code, "send_schedule_drift");
        let a = annotations(
            h,
            d,
            Phase::Warmup,
            (100, 2100),
            true,
            true,
            &mut Seen::default(),
        );
        assert_eq!(a[0].severity, Severity::Warn);
        assert!(
            a.iter()
                .all(|a| a.phase == Some(Phase::Warmup) && a.to_ms.unwrap() <= a.t_ms)
        );
        h.cap_duration = Duration::ZERO;
        h.sends = 98;
        h.max_drift = Duration::from_millis(50);
        h.drift_samples = 0;
        assert!(
            annotations(
                h,
                d,
                Phase::Measure,
                (100, 2100),
                true,
                false,
                &mut Seen::default()
            )
            .is_empty()
        );
        d.config.rate_tolerance_pct = 1;
        assert_eq!(
            annotations(
                h,
                d,
                Phase::Measure,
                (100, 2100),
                true,
                false,
                &mut Seen::default()
            )[0]
            .code,
            "rate_not_achieved"
        );
    }
}
