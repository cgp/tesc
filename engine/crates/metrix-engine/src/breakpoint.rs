//! Independently measured rate steps; no histogram pooling across a ramp.
use crate::{Output, Plan, Report};
use metrix_metrics::{
    Severity,
    aggregation::Window,
    events::SloVerdict,
    stats::{Percentile, percentiles},
};
use metrix_plan::mix::{Breakpoint, StopOn};
use serde::Serialize;
use std::{future::Future, time::Duration};
use tokio::sync::mpsc;

/// The fewest observations a proportion may be drawn from. Below it, a share says
/// more about the window than about the run (§12.1 draws the same line for tails).
const SHARE_FLOOR: u64 = 100;

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
    pub refinement: bool,
    pub statuses: std::collections::BTreeMap<String, u64>,
    pub errors: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Search {
    pub steps: Vec<Step>,
    pub stopped_because: String,
    pub max_sustained_rate: Option<f64>,
    pub knee: Option<f64>,
    pub cliff: Option<f64>,
    pub bracket: Option<[f64; 2]>,
    pub limiting_resource: Option<String>,
    pub resource_attribution: String,
    pub recovery_ms: Option<u64>,
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
    if b.refine {
        plan.final_settle = settle.max(b.step_recovery.map_or(Duration::ZERO, |d| d.as_duration()));
    }
    let clock = tokio::time::Instant::now();
    let mut search = Search {
        stopped_because: "max_rate".into(),
        ..Search::default()
    };
    let mut last = Report::default();
    let mut verdicts: Vec<SloVerdict> = Vec::new();
    for (index, rate) in rates.iter().copied().enumerate() {
        plan.rate = rate;
        plan.headroom_ratio = plan.headroom_for(rate);
        plan.duration = b.step_duration.as_duration();
        plan.baseline = if index == 0 { baseline } else { Duration::ZERO };
        plan.settle = if index + 1 == rates.len() {
            plan.final_settle
        } else {
            b.step_recovery.map_or(Duration::ZERO, |d| d.as_duration())
        };
        if let Some(output) = output {
            output.step_start(&plan);
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
        crate::slo::merge(&mut verdicts, &last.slo);
        let mut step = summarize(&last, rate, from_ms, to_ms, plan.duration);
        step.stop = last
            .stopped_because
            .clone()
            .or_else(|| assess(&last, b.stop_on, plan.breakpoint_baseline_p99))
            .or_else(|| slo_stop(&last.slo))
            .or_else(|| unsupported_stop(b.stop_on, &step));
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
    let valid = !last.generator_limited
        && !last.interrupted
        && search.stopped_because != "insufficient_samples";
    if valid {
        search.max_sustained_rate = search
            .steps
            .iter()
            .filter(|s| {
                s.stop.is_none()
                    && s.measured_seconds >= b.step_duration.as_duration().as_secs_f64()
            })
            .map(|s| s.rate)
            .reduce(f64::max);
        if let (Some(low), Some(bad)) = (
            search.max_sustained_rate,
            search.steps.last().filter(|s| s.stop.is_some()),
        ) {
            let high = bad.rate;
            search.bracket = Some([low, high]);
            if b.refine && low < high {
                let midpoint = low + (high - low) / 2.0;
                plan.rate = midpoint;
                plan.headroom_ratio = plan.headroom_for(midpoint);
                plan.refinement = true;
                plan.baseline = Duration::ZERO;
                plan.settle = settle;
                if let Some(output) = output {
                    output.step_start(&plan);
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
                crate::slo::merge(&mut verdicts, &last.slo);
                let mut step = summarize(&last, midpoint, from_ms, to_ms, plan.duration);
                step.refinement = true;
                step.stop = last
                    .stopped_because
                    .clone()
                    .or_else(|| assess(&last, b.stop_on, plan.breakpoint_baseline_p99))
                    .or_else(|| slo_stop(&last.slo))
                    .or_else(|| unsupported_stop(b.stop_on, &step));
                last.generator_limited |= step.stop.as_deref() == Some("generator_limited");
                if last.interrupted {
                    search.stopped_because = "interrupted".into();
                } else if last.generator_limited {
                    search.stopped_because = "generator_limited".into();
                } else if step.stop.as_deref() == Some("insufficient_samples") {
                    search.stopped_because = "insufficient_samples".into();
                } else if step.stop.is_none() {
                    search.max_sustained_rate = Some(midpoint);
                    search.bracket = Some([midpoint, high]);
                } else {
                    search.bracket = Some([low, midpoint]);
                }
                if let Some(output) = output {
                    output.note(
                        "breakpoint_step",
                        Severity::Info,
                        serde_json::to_value(&step).unwrap(),
                    );
                }
                search.steps.push(step);
            }
        }
        search.knee = search
            .steps
            .iter()
            .filter(|s| {
                s.p99.value_us.is_some_and(|us| {
                    b.stop_on
                        .p99_latency_ms
                        .is_some_and(|ms| us as f64 > ms as f64 * 1000.0)
                        || b.stop_on
                            .p99_multiple_of_baseline
                            .zip(plan.breakpoint_baseline_p99)
                            .is_some_and(|(factor, base)| us as f64 > base as f64 * factor)
                })
            })
            .map(|s| s.rate)
            .reduce(f64::min);
        search.cliff = search
            .steps
            .iter()
            .filter(|s| {
                (s.requests >= SHARE_FLOOR
                    && s.error_rate
                        .zip(b.stop_on.error_rate)
                        .is_some_and(|(actual, limit)| actual > limit))
                    || s.stop.as_deref() == Some("rate_shortfall_pct")
            })
            .map(|s| s.rate)
            .reduce(f64::min);
    }
    if last.generator_limited
        || last.interrupted
        || search.stopped_because == "insufficient_samples"
    {
        search.max_sustained_rate = None;
        search.knee = None;
        search.cliff = None;
        search.bracket = None;
    }
    search.resource_attribution = "Unavailable in standalone engine; target host metrics and recovery require API observation.".into();
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
    last.slo = verdicts;
    last.breakpoint = Some(search);
    Ok(last)
}

fn summarize(report: &Report, rate: f64, from_ms: u64, to_ms: u64, _duration: Duration) -> Step {
    let c = &report.metrics.counters;
    let h = report.diagnostics.measure;
    let (requests, failed) = crate::slo::request_counts(&report.metrics);
    Step {
        rate,
        from_ms: report.measured_from_ms.unwrap_or(from_ms),
        to_ms: report.measured_from_ms.map_or(to_ms, |from| {
            from.saturating_add(report.diagnostics.measure.observed.as_millis() as u64)
        }),
        measured_seconds: report.diagnostics.measure.observed.as_secs_f64(),
        iterations: h.offered,
        completed: report.metrics.chains.values().map(|c| c.completed).sum(),
        requests,
        failed,
        error_rate: (requests > 0).then(|| failed as f64 / requests as f64),
        achieved_rate: report
            .metrics
            .chains
            .values()
            .map(|c| c.started)
            .sum::<u64>() as f64
            / report.diagnostics.measure.observed.as_secs_f64().max(0.001),
        p99: percentiles(&report.metrics.total).p99,
        stop: None,
        refinement: false,
        statuses: c
            .statuses
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(s, n)| (s.to_string(), *n))
            .collect(),
        errors: c
            .errors
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(i, n)| {
                (
                    [
                        "dns",
                        "connect",
                        "tls",
                        "protocol",
                        "send",
                        "body",
                        "timeout",
                        "extraction",
                        "assertion",
                        "generation",
                        "unauthorized",
                    ][i]
                        .to_owned(),
                    *n,
                )
            })
            .collect(),
    }
}

/// Generator evidence takes precedence over any target threshold at the same tick.
pub(crate) fn assess(report: &Report, stop: StopOn, baseline_p99: Option<u64>) -> Option<String> {
    let h = report.diagnostics.measure;
    if h.observed < Duration::from_secs(1) {
        return None;
    }
    if limiting(report, true).is_some() {
        return Some("generator_limited".into());
    }
    let (requests, failed) = crate::slo::request_counts(&report.metrics);
    if requests >= SHARE_FLOOR
        && stop
            .error_rate
            .is_some_and(|n| failed as f64 / requests as f64 > n)
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

/// A step's verdicts, read as a reason to stop climbing.
///
/// A breach is a finding: the rate that produced it is past what the plan asked
/// for. A threshold the samples cannot support is the absence of a finding, and a
/// capacity claim resting on one would be a guess, so the search stops without
/// claiming a rate (§16).
fn slo_stop(verdicts: &[SloVerdict]) -> Option<String> {
    if verdicts.iter().any(|verdict| !verdict.passed) {
        Some("slo".into())
    } else if verdicts.iter().any(|verdict| !verdict.supported) {
        Some("insufficient_samples".into())
    } else {
        None
    }
}

/// The step's own stop conditions, judged against what the step actually measured.
/// A threshold with nothing to compare against has not been met and has not been
/// missed.
fn unsupported_stop(stop: StopOn, step: &Step) -> Option<String> {
    let tail_wanted = stop.p99_latency_ms.is_some() || stop.p99_multiple_of_baseline.is_some();
    ((tail_wanted && step.p99.value_us.is_none())
        || (stop.error_rate.is_some() && step.requests < SHARE_FLOOR))
        .then(|| "insufficient_samples".to_owned())
}

/// What makes a result untrustworthy, and the evidence that made it so (§16).
///
/// A capacity search and a fixed run are not asked the same question, so they are
/// not invalidated by the same things. A search extrapolates a number from the
/// window it measured, so send drift and a chain that broke both turn that number
/// into a guess. A fixed run was asked for a timeline: it delivered it or it did
/// not. Drift within it is a warning the run already carries, and a chain that
/// broke is a finding about the service rather than about the generator that
/// reported it.
pub(crate) fn limiting(report: &Report, searching: bool) -> Option<&'static str> {
    let h = report.diagnostics.measure;
    if h.observed < Duration::from_secs(1) {
        return None;
    }
    let tolerance = f64::from(report.diagnostics.config.rate_tolerance_pct);
    // A share needs a denominator. Two missed arrivals out of fifty is 4% and is a
    // scheduler tick on a busy box, not a generator that cannot sustain the rate --
    // and this is the same floor a capacity search already puts under the error
    // rate it stops on. Counts, durations and outright failures below need none: an
    // exhausted socket is an exhausted socket at any sample size.
    let over = |count: u64| {
        h.offered >= SHARE_FLOOR && count as f64 / h.offered as f64 * 100.0 > tolerance
    };
    let errors = &report.metrics.counters.errors;
    let generation_failed = report
        .metrics
        .generation
        .values()
        .any(|g| g.failed > 0 || g.duration.overflow > 0)
        || errors[metrix_metrics::aggregation::Cause::Generation as usize] > 0;
    Some(if over(h.skipped_late) {
        "arrivals_missed"
    } else if over(h.skipped_concurrency) {
        "concurrency_cap"
    } else if h.cap_duration.saturating_mul(4) > h.observed {
        "held_at_the_cap"
    } else if h.skipped_connections > 0 {
        "connections_exhausted"
    } else if generation_failed {
        "generation_failed"
    } else if searching && h.max_drift > report.diagnostics.config.drift_threshold {
        "send_drift"
    } else if searching && errors[metrix_metrics::aggregation::Cause::Extraction as usize] > 0 {
        "chain_broken"
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PhaseHealth;
    use metrix_metrics::aggregation::Cause;

    fn measured(health: PhaseHealth) -> Report {
        let mut report = Report::default();
        report.diagnostics.measure = PhaseHealth {
            observed: Duration::from_secs(10),
            offered: 1000,
            ..health
        };
        report
    }

    /// The sentence §16 turns on: a fixed run was asked for a timeline and either
    /// delivered it or did not. Drift within a delivered one is a warning the run
    /// already carries, and a chain that broke is a finding about the service, not
    /// a confession that the generator was too small.
    #[test]
    fn a_search_and_a_fixed_run_are_invalidated_by_different_evidence() {
        let drifting = measured(PhaseHealth {
            max_drift: Duration::from_millis(50),
            ..PhaseHealth::default()
        });
        assert_eq!(limiting(&drifting, true), Some("send_drift"));
        assert_eq!(limiting(&drifting, false), None);

        let mut broken = measured(PhaseHealth::default());
        broken.metrics.counters.errors[Cause::Extraction as usize] = 4;
        assert_eq!(limiting(&broken, true), Some("chain_broken"));
        assert_eq!(limiting(&broken, false), None);
    }

    /// Everything that means the generator, rather than the service, set the pace.
    /// These invalidate either kind of run: the rate on the page was never offered.
    #[test]
    fn the_generator_falling_behind_invalidates_both() {
        for (health, evidence) in [
            (
                PhaseHealth {
                    skipped_late: 50,
                    ..PhaseHealth::default()
                },
                "arrivals_missed",
            ),
            (
                PhaseHealth {
                    skipped_concurrency: 50,
                    ..PhaseHealth::default()
                },
                "concurrency_cap",
            ),
            (
                PhaseHealth {
                    cap_duration: Duration::from_secs(3),
                    ..PhaseHealth::default()
                },
                "held_at_the_cap",
            ),
            (
                PhaseHealth {
                    skipped_connections: 1,
                    ..PhaseHealth::default()
                },
                "connections_exhausted",
            ),
        ] {
            let report = measured(health);
            assert_eq!(limiting(&report, true), Some(evidence));
            assert_eq!(limiting(&report, false), Some(evidence));
        }
        let mut generating = measured(PhaseHealth::default());
        generating.metrics.counters.errors[Cause::Generation as usize] = 1;
        assert_eq!(limiting(&generating, true), Some("generation_failed"));
        assert_eq!(limiting(&generating, false), Some("generation_failed"));
    }

    /// Under a busy CI box, one 1s window at 50/s missing two arrivals reads as
    /// four percent, and four percent of nothing much is still nothing much. The
    /// share gets the same floor the error rate beside it already has.
    #[test]
    fn a_share_drawn_from_too_few_arrivals_is_not_evidence() {
        let mut report = measured(PhaseHealth {
            skipped_late: 2,
            ..PhaseHealth::default()
        });
        report.diagnostics.measure.offered = 50;
        assert_eq!(limiting(&report, false), None);

        report.diagnostics.measure.offered = SHARE_FLOOR;
        report.diagnostics.measure.skipped_late = 4;
        assert_eq!(limiting(&report, false), Some("arrivals_missed"));
    }

    /// A window too short to hold a judgement does not produce one.
    #[test]
    fn a_window_under_a_second_is_not_judged_at_all() {
        let mut report = measured(PhaseHealth {
            skipped_late: 500,
            ..PhaseHealth::default()
        });
        assert_eq!(limiting(&report, false), Some("arrivals_missed"));
        report.diagnostics.measure.observed = Duration::from_millis(999);
        assert_eq!(limiting(&report, false), None);
    }
}
