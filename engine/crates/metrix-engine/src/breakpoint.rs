//! Independently measured rate steps; no histogram pooling across a ramp.
use crate::{Output, Plan, Report};
use metrix_metrics::{
    Severity,
    aggregation::Window,
    stats::{Percentile, percentiles},
};
use metrix_plan::mix::{Breakpoint, StopOn};
use serde::Serialize;
use std::{future::Future, time::Duration};
use tokio::sync::mpsc;

#[derive(Debug, Serialize)]
pub struct Step {
    pub rate: f64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub measured_seconds: f64,
    pub iterations: u64,
    pub completed: u64,
    pub requests: u64,
    pub failed: u64,
    pub error_rate: Option<f64>,
    pub achieved_rate: f64,
    pub p99: Percentile,
    pub stop: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Search {
    pub steps: Vec<Step>,
    pub stopped_because: String,
}

pub(crate) fn rates(b: &Breakpoint) -> Result<Vec<f64>, String> {
    let error = || {
        "mix.json#/load/breakpoint: require finite positive rates, max >= start, exactly one positive step_rate or step_factor > 1, and nonzero step_duration".to_owned()
    };
    if !b.start_rate.is_finite()
        || b.start_rate <= 0.0
        || !b.max_rate.is_finite()
        || b.max_rate < b.start_rate
        || b.step_duration.is_zero()
        || !matches!((b.step_rate, b.step_factor), (Some(n), None) if n.is_finite() && n > 0.0)
            && !matches!((b.step_rate, b.step_factor), (None, Some(n)) if n.is_finite() && n > 1.0)
    {
        return Err(error());
    }
    let mut rates = vec![b.start_rate];
    while *rates.last().unwrap() < b.max_rate {
        if rates.len() >= 10_000 {
            return Err("mix.json#/load/breakpoint: at most 10000 steps are allowed".into());
        }
        let rate = *rates.last().unwrap();
        let next = b
            .step_rate
            .map_or_else(|| rate * b.step_factor.unwrap(), |n| rate + n)
            .min(b.max_rate);
        if next <= rate {
            return Err(error());
        }
        rates.push(next);
    }
    let stop = b.stop_on;
    if stop
        .error_rate
        .is_some_and(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
        || stop
            .rate_shortfall_pct
            .is_some_and(|n| !n.is_finite() || !(0.0..100.0).contains(&n))
        || stop
            .p99_multiple_of_baseline
            .is_some_and(|n| !n.is_finite() || n <= 1.0)
        || stop.p99_latency_ms == Some(0)
    {
        return Err("mix.json#/load/breakpoint/stop_on: invalid threshold".into());
    }
    for &rate in &rates {
        crate::schedule::Schedule::validate(rate, b.step_duration.as_duration())?;
    }
    Ok(rates)
}

pub(crate) async fn run(
    mut plan: Plan,
    shutdown: impl Future<Output = ()>,
    snapshots: Option<mpsc::Sender<Window>>,
    output: Option<&Output>,
) -> Result<Report, String> {
    tokio::pin!(shutdown);
    let b = plan.breakpoint.clone().expect("breakpoint plan");
    let mut b = b;
    let mut capped = false;
    if let Some(profile) = &plan.machine_profile {
        if !plan.allow_generator_limited {
            let ceiling = profile.ceiling(plan.worker_threads) * 0.9 / plan.request_factor();
            if b.max_rate > ceiling {
                b.max_rate = ceiling;
                capped = true;
            }
        }
    }
    let rates = rates(&b)?;
    let baseline = plan.baseline;
    let settle = plan.settle;
    let clock = tokio::time::Instant::now();
    let mut search = Search {
        stopped_because: "max_rate".into(),
        ..Search::default()
    };
    let mut last = Report::default();
    for (index, rate) in rates.iter().copied().enumerate() {
        plan.rate = rate;
        plan.headroom_ratio = plan.headroom_for(rate);
        plan.duration = b.step_duration.as_duration();
        plan.baseline = if index == 0 { baseline } else { Duration::ZERO };
        plan.settle = if index + 1 == rates.len() {
            settle
        } else {
            b.step_recovery.map_or(Duration::ZERO, |d| d.as_duration())
        };
        if let Some(output) = output {
            output.step_start(&plan)?;
        }
        let from_ms = output.map_or(clock.elapsed().as_millis() as u64, |o| o.elapsed());
        last = Box::pin(crate::execution::run(
            &plan,
            &mut shutdown,
            snapshots.clone(),
            output,
        ))
        .await?;
        let to_ms = output.map_or(clock.elapsed().as_millis() as u64, |o| o.elapsed());
        let mut step = summarize(&last, rate, from_ms, to_ms, plan.duration);
        step.stop = last
            .stopped_because
            .clone()
            .or_else(|| assess(&last, b.stop_on, plan.breakpoint_baseline_p99));
        if step.stop.is_none()
            && (((b.stop_on.p99_latency_ms.is_some()
                || b.stop_on.p99_multiple_of_baseline.is_some())
                && step.p99.value_us.is_none())
                || (b.stop_on.error_rate.is_some() && step.requests < 100))
        {
            step.stop = Some("insufficient_samples".into());
        }
        if index == 0 {
            plan.breakpoint_baseline_p99 = step.p99.value_us;
        }
        let stopping = step.stop.clone();
        if let Some(output) = output {
            output.note(
                "breakpoint_step",
                Severity::Info,
                serde_json::to_value(&step).unwrap(),
            );
        }
        search.steps.push(step);
        plan.iteration_base += last.admitted;
        if let Some(reason) = stopping {
            last.generator_limited |= reason == "generator_limited";
            search.stopped_because = reason;
            break;
        }
        if last.interrupted {
            search.stopped_because = "interrupted".into();
            break;
        }
    }
    if capped && search.stopped_because == "max_rate" {
        search.stopped_because = "calibrated_ceiling".into();
    }
    if last.generator_limited {
        if let Some(output) = output {
            output.note(
                "generator_limited",
                Severity::Invalid,
                serde_json::json!({"capacity_claim":null}),
            );
        }
    }
    if let Some(output) = output {
        output.note(
            "breakpoint_report",
            Severity::Info,
            serde_json::to_value(&search).unwrap(),
        );
    }
    last.stopped_because = Some(search.stopped_because.clone());
    last.breakpoint = Some(search);
    Ok(last)
}

fn summarize(report: &Report, rate: f64, from_ms: u64, to_ms: u64, _duration: Duration) -> Step {
    let c = &report.metrics.counters;
    let h = report.diagnostics.measure;
    Step {
        rate,
        from_ms: report.measured_from_ms.unwrap_or(from_ms),
        to_ms: report.measured_from_ms.map_or(to_ms, |from| {
            from.saturating_add(report.diagnostics.measure.observed.as_millis() as u64)
        }),
        measured_seconds: report.diagnostics.measure.observed.as_secs_f64(),
        iterations: h.offered,
        completed: report.metrics.chains.values().map(|c| c.completed).sum(),
        requests: c.completed,
        failed: c.failed,
        error_rate: (c.completed > 0).then(|| c.failed as f64 / c.completed as f64),
        achieved_rate: report
            .metrics
            .chains
            .values()
            .map(|c| c.started)
            .sum::<u64>() as f64
            / report.diagnostics.measure.observed.as_secs_f64().max(0.001),
        p99: percentiles(&report.metrics.total).p99,
        stop: None,
    }
}

/// Generator evidence takes precedence over any target threshold at the same tick.
pub(crate) fn assess(report: &Report, stop: StopOn, baseline_p99: Option<u64>) -> Option<String> {
    let h = report.diagnostics.measure;
    if h.observed < Duration::from_secs(1) {
        return None;
    }
    let tolerance = f64::from(report.diagnostics.config.rate_tolerance_pct);
    let late_pct = h.skipped_late as f64 / h.offered.max(1) as f64 * 100.0;
    let generation_bad = report
        .metrics
        .generation
        .values()
        .any(|g| g.failed > 0 || g.duration.overflow > 0)
        || report.metrics.counters.errors[metrix_metrics::aggregation::Cause::Generation as usize]
            > 0
        || report.metrics.counters.errors[metrix_metrics::aggregation::Cause::Extraction as usize]
            > 0;
    let cap_pct = h.skipped_concurrency as f64 / h.offered.max(1) as f64 * 100.0;
    if late_pct > tolerance
        || cap_pct > tolerance
        || h.max_drift > report.diagnostics.config.drift_threshold
        || h.cap_duration.saturating_mul(4) > h.observed
        || h.skipped_connections > 0
        || generation_bad
    {
        return Some("generator_limited".into());
    }
    let c = &report.metrics.counters;
    if c.completed >= 100
        && stop
            .error_rate
            .is_some_and(|n| c.failed as f64 / c.completed as f64 > n)
    {
        return Some("error_rate".into());
    }
    let p99 = percentiles(&report.metrics.total).p99.value_us;
    if p99
        .zip(stop.p99_latency_ms)
        .is_some_and(|(us, ms)| us as f64 > ms as f64 * 1000.0)
    {
        return Some("p99_latency_ms".into());
    }
    if let (Some(us), Some(base), Some(factor)) = (p99, baseline_p99, stop.p99_multiple_of_baseline)
    {
        if us as f64 > base as f64 * factor {
            return Some("p99_multiple_of_baseline".into());
        }
    }
    let started: u64 = report.metrics.chains.values().map(|c| c.started).sum();
    let shortfall = h.offered.saturating_sub(started) as f64 / h.offered.max(1) as f64 * 100.0;
    if stop.rate_shortfall_pct.is_some_and(|n| shortfall > n) {
        return Some("rate_shortfall_pct".into());
    }
    None
}
