//! Thresholds judged against the populations a run actually measured (§16).
//!
//! Every target and every rate step is judged on its own numbers and the verdicts
//! are merged afterwards, keeping the worst observation and every breach. A
//! threshold the samples cannot support is advisory rather than a failure: the run
//! did not find that the service was fast enough, it found that it could not tell.

use metrix_metrics::{
    aggregation::{Accumulator, Distribution},
    events::{Bound, SloVerdict},
    stats::{Percentile, Percentiles, percentiles},
};
use metrix_plan::mix::Slo;

const METRICS: [&str; 6] = [
    "p50_latency_ms",
    "p95_latency_ms",
    "p99_latency_ms",
    "p99_9_latency_ms",
    "error_rate",
    "achieved_rate",
];

pub(crate) fn validate(slos: &[Slo], chains: &[&str]) -> Result<(), String> {
    for (index, slo) in slos.iter().enumerate() {
        let at = format!("mix.json#/slo/{index}");
        let metric = slo.metric.as_str();
        require(
            METRICS.contains(&metric),
            &format!(
                "{at}/metric: not a load metric; one of {}",
                METRICS.join(", ")
            ),
        )?;
        if let Some(name) = &slo.chain {
            require(
                chains.contains(&name.as_str()),
                &format!(
                    "{at}/chain: no chain named {name:?}; one of {}",
                    chains.join(", ")
                ),
            )?;
        }
        // A threshold with no bound is a threshold nothing can fail: a plan that
        // asks a question and throws the answer away.
        require(
            slo.min.is_some() || slo.max.is_some(),
            &format!("{at}: a threshold needs a min, a max, or both"),
        )?;
        for bound in slo.min.iter().chain(slo.max.iter()) {
            require(
                bound.is_finite() && *bound >= 0.0,
                &format!("{at}: bounds must be finite and not negative"),
            )?;
            require(
                metric != "error_rate" || *bound <= 1.0,
                &format!("{at}: error_rate is a fraction, so its bounds lie in [0, 1]"),
            )?;
        }
        require(
            slo.min.zip(slo.max).is_none_or(|(min, max)| min <= max),
            &format!("{at}: min {:?} is above max {:?}", slo.min, slo.max),
        )?;
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

/// What one threshold is about: one chain's steps, or every chain's.
///
/// Chain counters count iterations, so a request error rate has to be built from
/// the terminal outcomes of the steps -- the same population the latencies come
/// from, or the two would disagree about the same run.
#[derive(Default)]
struct Population {
    requests: u64,
    failed: u64,
    iterations: u64,
}

fn population(metrics: &Accumulator, chain: Option<&str>) -> Population {
    let mut found = Population::default();
    for (name, counts) in &metrics.chains {
        if chain.is_some_and(|wanted| *name != wanted) {
            continue;
        }
        found.iterations += counts.started;
        for step in counts.steps.values() {
            found.requests += step.completed + step.failed;
            found.failed += step.failed;
        }
    }
    found
}

/// The terminal request outcomes of a whole run. The breakpoint step summary and
/// its stop conditions judge the same population the thresholds do, or a step
/// could stop on an error rate no verdict agrees with.
pub(crate) fn request_counts(metrics: &Accumulator) -> (u64, u64) {
    let found = population(metrics, None);
    (found.requests, found.failed)
}

fn latency(metrics: &Accumulator, chain: Option<&str>) -> Distribution {
    let mut total = Distribution::default();
    match chain.and_then(|name| metrics.chains.get(name)) {
        Some(counts) => {
            for step in counts.steps.values() {
                total.merge(&step.total);
            }
        }
        // Run-wide latency is already merged once, on the hot path. Merging the
        // steps again would reach the same population by a longer route.
        None if chain.is_none() => total.merge(&metrics.total),
        None => {}
    }
    total
}

fn tail<'a>(metric: &str, latency: &'a Percentiles) -> Option<&'a Percentile> {
    Some(match metric {
        "p50_latency_ms" => &latency.p50,
        "p95_latency_ms" => &latency.p95,
        "p99_latency_ms" => &latency.p99,
        "p99_9_latency_ms" => &latency.p99_9,
        _ => return None,
    })
}

/// `None` where the population cannot answer: an unmeasured window, no requests,
/// or a tail the sample count does not support.
fn observe(slo: &Slo, metrics: &Accumulator, seconds: f64) -> Option<f64> {
    let chain = slo.chain.as_deref();
    let found = population(metrics, chain);
    match slo.metric.as_str() {
        "error_rate" => (found.requests > 0).then(|| found.failed as f64 / found.requests as f64),
        "achieved_rate" => (seconds > 0.0).then(|| found.iterations as f64 / seconds),
        metric => tail(metric, &percentiles(&latency(metrics, chain)))?
            .value_us
            .map(|us| us as f64 / 1000.0),
    }
}

pub(crate) fn evaluate(slos: &[Slo], metrics: &Accumulator, seconds: f64) -> Vec<SloVerdict> {
    let mut verdicts = Vec::new();
    for slo in slos {
        let observed = observe(slo, metrics, seconds);
        for (bound, threshold) in bounds(slo) {
            verdicts.push(SloVerdict {
                metric: slo.metric.clone(),
                chain: slo.chain.clone(),
                bound,
                // Inclusive: a plan asking for at most 200ms is not breached by 200ms.
                passed: observed.is_none_or(|value| match bound {
                    Bound::Min => value >= threshold,
                    Bound::Max => value <= threshold,
                }),
                observed: observed.unwrap_or(0.0),
                threshold,
                supported: observed.is_some(),
            });
        }
    }
    verdicts
}

/// One verdict per bound, in a fixed order, so the merge across targets and rate
/// steps can line them up positionally.
fn bounds(slo: &Slo) -> impl Iterator<Item = (Bound, f64)> {
    slo.min
        .map(|n| (Bound::Min, n))
        .into_iter()
        .chain(slo.max.map(|n| (Bound::Max, n)))
}

/// What each threshold was judged on, for the annotation that accompanies the
/// verdicts. A verdict that reports itself unsupported without saying how many
/// samples it had is not something a reader can act on.
pub(crate) fn evidence(
    slos: &[Slo],
    metrics: &Accumulator,
    seconds: f64,
) -> Vec<serde_json::Value> {
    slos.iter()
        .map(|slo| {
            let chain = slo.chain.as_deref();
            let found = population(metrics, chain);
            let support = match slo.metric.as_str() {
                "achieved_rate" => {
                    serde_json::json!({"iterations": found.iterations, "seconds": seconds})
                }
                metric => match tail(metric, &percentiles(&latency(metrics, chain))) {
                    Some(percentile) => serde_json::json!(percentile),
                    None => serde_json::Value::Null,
                },
            };
            serde_json::json!({
                "metric": slo.metric, "chain": slo.chain, "requests": found.requests,
                "failed": found.failed, "support": support,
            })
        })
        .collect()
}

/// Fold one target's or one rate step's verdicts into the run's.
///
/// The run keeps the worst supported observation and every breach: a service that
/// failed one target and recovered on the next did fail, and reporting the last
/// number measured would hide it.
pub(crate) fn merge(into: &mut Vec<SloVerdict>, next: &[SloVerdict]) {
    if into.is_empty() {
        into.extend_from_slice(next);
        return;
    }
    for (kept, new) in into.iter_mut().zip(next) {
        kept.passed &= new.passed;
        let worse = match new.bound {
            Bound::Min => new.observed < kept.observed,
            Bound::Max => new.observed > kept.observed,
        };
        // Nothing supported yet means `observed` holds the sentinel zero rather than
        // a measurement, and any real number beats it in either direction.
        if new.supported && (!kept.supported || worse) {
            kept.observed = new.observed;
        }
        kept.supported |= new.supported;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn slo(metric: &str, max: f64) -> Slo {
        serde_json::from_value(json!({"metric": metric, "max": max})).unwrap()
    }

    /// 1000 samples support a p99 and 999 do not; the floor lives in `stats/` and an
    /// SLO must not become a second opinion about it.
    #[test]
    fn a_tail_is_advisory_until_the_samples_support_it() {
        let specs = [slo("p99_latency_ms", 1.0), slo("error_rate", 0.01)];
        let mut metrics = Accumulator::default();
        for _ in 0..999 {
            metrics.total.record(Duration::from_millis(1));
        }
        let verdicts = evaluate(&specs, &metrics, 1.0);
        assert!(!verdicts[0].supported && verdicts[0].passed);
        metrics.total.record(Duration::from_millis(1));
        let verdicts = evaluate(&specs, &metrics, 1.0);
        assert!(verdicts[0].supported && verdicts[0].passed);

        let step = metrics
            .chains
            .entry("chain")
            .or_default()
            .steps
            .entry("get")
            .or_default();
        step.completed = 98;
        step.failed = 2;
        let verdicts = evaluate(&specs, &metrics, 1.0);
        assert!(verdicts[1].supported && !verdicts[1].passed);
        assert_eq!(verdicts[1].observed, 0.02);
    }

    #[test]
    fn a_breach_survives_later_unsupported_and_healthy_populations() {
        let specs = [slo("p99_latency_ms", 10.0)];
        let mut failed = Accumulator::default();
        let mut healthy = Accumulator::default();
        for _ in 0..1000 {
            failed.total.record(Duration::from_millis(20));
            healthy.total.record(Duration::from_millis(1));
        }
        let mut verdicts = Vec::new();
        merge(&mut verdicts, &evaluate(&specs, &failed, 1.0));
        merge(
            &mut verdicts,
            &evaluate(&specs, &Accumulator::default(), 1.0),
        );
        merge(&mut verdicts, &evaluate(&specs, &healthy, 1.0));
        assert!(!verdicts[0].passed);
        assert!(verdicts[0].supported);
        assert!(verdicts[0].observed >= 20.0);
    }

    /// The sentinel zero is not an observation, and a floor must not mistake it for
    /// the worst one seen.
    #[test]
    fn the_worst_observation_of_a_floor_is_the_lowest_one_measured() {
        let specs: Vec<Slo> =
            serde_json::from_value(json!([{"metric": "achieved_rate", "min": 5.0}])).unwrap();
        let mut slow = Accumulator::default();
        slow.chains.entry("ping").or_default().started = 3;
        let mut fast = Accumulator::default();
        fast.chains.entry("ping").or_default().started = 40;

        let mut verdicts = Vec::new();
        merge(
            &mut verdicts,
            &evaluate(&specs, &Accumulator::default(), 0.0),
        );
        merge(&mut verdicts, &evaluate(&specs, &fast, 1.0));
        merge(&mut verdicts, &evaluate(&specs, &slow, 1.0));
        assert_eq!(verdicts[0].observed, 3.0);
        assert!(!verdicts[0].passed && verdicts[0].supported);
    }

    #[test]
    fn a_chain_scope_and_both_bounds_use_their_own_population() {
        let specs: Vec<Slo> = serde_json::from_value(json!([
            {"metric": "achieved_rate", "chain": "slow", "min": 4, "max": 4},
            {"metric": "error_rate", "chain": "slow", "min": 0.125, "max": 0.125},
            {"metric": "p99_latency_ms", "chain": "slow", "max": 10},
            {"metric": "p99_latency_ms", "max": 10}
        ]))
        .unwrap();
        let mut metrics = Accumulator::default();
        let chain = metrics.chains.entry("slow").or_default();
        chain.started = 8;
        let step = chain.steps.entry("get").or_default();
        step.completed = 7;
        step.failed = 1;
        for _ in 0..1000 {
            step.total.record(Duration::from_millis(50));
        }
        for _ in 0..1000 {
            metrics.total.record(Duration::from_millis(1));
        }

        let verdicts = evaluate(&specs, &metrics, 2.0);
        // Both bounds of both scoped thresholds hold exactly: eight iterations over
        // two seconds is 4/s, and one failure in eight requests is 0.125.
        assert!(verdicts[..4].iter().all(|v| v.supported && v.passed));
        assert_eq!(verdicts[0].bound, Bound::Min);
        assert_eq!(verdicts[1].bound, Bound::Max);
        // The chain was slow and the run as a whole was not, which is the point of
        // being able to scope a threshold at all.
        assert!(verdicts[4].supported && !verdicts[4].passed);
        assert!(verdicts[5].supported && verdicts[5].passed);

        let evidence = evidence(&specs, &metrics, 2.0);
        assert_eq!(evidence[0]["requests"], json!(8));
        assert_eq!(evidence[0]["support"]["iterations"], json!(8));
        assert_eq!(evidence[2]["support"]["count"], json!(1000));
    }

    #[test]
    fn invalid_metrics_bounds_and_chain_scopes_are_located() {
        for spec in [
            json!({"metric": "unknown", "max": 1}),
            json!({"metric": "error_rate", "max": 2}),
            json!({"metric": "achieved_rate"}),
            json!({"metric": "p99_latency_ms", "min": 2, "max": 1}),
            json!({"metric": "p99_latency_ms", "chain": "missing", "max": 1}),
        ] {
            let error = validate(&[serde_json::from_value(spec).unwrap()], &["known"]).unwrap_err();
            assert!(error.starts_with("mix.json#/slo/0"), "{error}");
        }
    }
}
