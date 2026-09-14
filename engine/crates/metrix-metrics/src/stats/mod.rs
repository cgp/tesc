//! Load percentiles: actual tail support and binomial order-statistic intervals.
//! Host-sample statistics in the control plane describe a different population.

use crate::aggregation::Distribution;
use serde::{Deserialize, Serialize};
use statrs::distribution::{Binomial, DiscreteCDF};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Suppressed,
    Crude,
    Stable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suppression {
    InsufficientSamples,
    HistogramOverflow,
    CountNotRepresentable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceInterval {
    pub level: f64,
    pub lower_us: u64,
    pub upper_us: u64,
    /// One-based order-statistic ranks, before HDR bucket expansion.
    pub lower_rank: u64,
    pub upper_rank: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Percentile {
    pub count: u64,
    pub overflow: u64,
    pub support: Support,
    pub suppression: Option<Suppression>,
    pub value_us: Option<u64>,
    pub ci95: Option<ConfidenceInterval>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Percentiles {
    pub p50: Percentile,
    pub p95: Percentile,
    pub p99: Percentile,
    pub p99_9: Percentile,
}

/// Each claim carries its own count; merging must happen before this calculation.
pub fn percentiles(distribution: &Distribution) -> Percentiles {
    Percentiles {
        p50: estimate(distribution, 2),
        p95: estimate(distribution, 20),
        p99: estimate(distribution, 100),
        p99_9: estimate(distribution, 1000),
    }
}

fn estimate(d: &Distribution, tail_denominator: u64) -> Percentile {
    let n = d.count();
    let suppression = if d.overflow > 0 {
        Some(Suppression::HistogramOverflow)
    } else if n > (1_u64 << 53) {
        Some(Suppression::CountNotRepresentable)
    } else if n < 10 * tail_denominator {
        Some(Suppression::InsufficientSamples)
    } else {
        None
    };
    let mut result = Percentile {
        count: n,
        overflow: d.overflow,
        support: Support::Suppressed,
        suppression,
        value_us: None,
        ci95: None,
    };
    if suppression.is_some() {
        return result;
    }
    result.support = if n >= 100 * tail_denominator {
        Support::Stable
    } else {
        Support::Crude
    };
    let q = 1.0 - 1.0 / tail_denominator as f64;
    let binomial = Binomial::new(q, n).expect("fixed percentile probability");
    // P(B < L) <= .025 and P(B >= U) <= .025 for B ~ Binomial(n, q).
    let lower_rank = inverse_cdf(&binomial, n, 0.025);
    let upper_rank = inverse_cdf(&binomial, n, 0.975) + 1;
    debug_assert!(lower_rank > 0 && upper_rank <= n);
    // Exact integer nearest-rank arithmetic avoids floating-point bucket selection.
    let rank = n - n / tail_denominator;
    result.value_us = Some(order_value(d, rank));
    result.ci95 = Some(ConfidenceInterval {
        level: 0.95,
        lower_us: d.hdr.lowest_equivalent(order_value(d, lower_rank)),
        upper_us: d.hdr.highest_equivalent(order_value(d, upper_rank)),
        lower_rank,
        upper_rank,
    });
    result
}

// Bounded binary search avoids the generic inverse-CDF implementation's unbounded
// bracket search. All sample counts admitted above are exactly representable as f64.
fn inverse_cdf(binomial: &Binomial, n: u64, probability: f64) -> u64 {
    let (mut low, mut high) = (0, n);
    while low < high {
        let mid = low + (high - low) / 2;
        if binomial.cdf(mid) >= probability {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

fn order_value(d: &Distribution, rank: u64) -> u64 {
    let mut count = 0;
    for bucket in d.hdr.iter_recorded() {
        count += bucket.count_since_last_iteration();
        if count >= rank {
            return bucket.value_iterated_to();
        }
    }
    unreachable!("rank within histogram sample count")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregation::MAX_LATENCY_US;
    use std::time::Duration;

    fn repeated(n: u64) -> Distribution {
        let mut d = Distribution::default();
        d.hdr.record_n(1000, n).unwrap();
        d
    }

    #[test]
    fn suppression_and_stability_use_exact_tail_thresholds() {
        for denominator in [2, 20, 100, 1000] {
            for (n, expected) in [
                (0, Support::Suppressed),
                (10 * denominator - 1, Support::Suppressed),
                (10 * denominator, Support::Crude),
                (100 * denominator - 1, Support::Crude),
                (100 * denominator, Support::Stable),
            ] {
                let p = estimate(&repeated(n), denominator);
                assert_eq!(p.count, n);
                assert_eq!(p.support, expected);
                assert_eq!(p.value_us.is_some(), expected != Support::Suppressed);
                assert_eq!(p.ci95.is_some(), expected != Support::Suppressed);
                if let Some(ci) = p.ci95 {
                    assert_eq!((ci.lower_us, ci.upper_us), (1000, 1000));
                }
            }
        }
    }

    #[test]
    fn floor_supports_coarse_p99_and_suppresses_p99_9() {
        let mut d = Distribution::default();
        for value in 1..=2250 {
            d.record(Duration::from_micros(value));
        }
        let p = percentiles(&d);
        assert_eq!(p.p50.support, Support::Stable);
        assert_eq!(p.p95.support, Support::Stable);
        assert_eq!(p.p99.support, Support::Crude);
        assert_eq!(p.p99_9.support, Support::Suppressed);
        assert_eq!(p.p99_9.value_us, None);
        let ci = p.p99.ci95.unwrap();
        assert_eq!((ci.lower_rank, ci.upper_rank), (2218, 2237));
        assert!(ci.lower_us <= 2218 && ci.upper_us >= 2237);
        assert!(p.p99.value_us.unwrap() >= 2228 && p.p99.value_us.unwrap() <= 2229);
    }

    #[test]
    fn exact_intervals_cover_at_least_95_percent_and_have_tight_equal_tails() {
        // Independent PMF recurrence for failures above the population quantile.
        // This verifies discrete coverage and off-by-one ranks, without using statrs.
        for (n, denominator) in [(20, 2), (200, 20), (1000, 100), (2250, 100), (10000, 1000)] {
            let q = 1.0 / denominator as f64;
            let mut probability = (1.0 - q).powi(n as i32);
            let mut cdf = Vec::new();
            let mut sum = 0.0;
            for k in 0..=n {
                sum += probability;
                cdf.push(sum);
                probability *= (n - k) as f64 / (k + 1) as f64 * q / (1.0 - q);
            }
            assert!((sum - 1.0).abs() < 1e-12);
            let ci = estimate(&repeated(n), denominator).ci95.unwrap();
            let lower_tail = 1.0 - cdf[(n - ci.lower_rank) as usize];
            let upper_tail = cdf[(n - ci.upper_rank) as usize];
            assert!(lower_tail <= 0.025 && upper_tail <= 0.025);
            assert!(1.0 - cdf[(n - ci.lower_rank - 1) as usize] > 0.025);
            assert!(cdf[(n - ci.upper_rank + 1) as usize] > 0.025);
        }
    }

    #[test]
    fn overflow_suppresses_even_a_large_recorded_population() {
        let mut d = repeated(100_000);
        d.record(Duration::from_micros(MAX_LATENCY_US + 1));
        let p = percentiles(&d);
        for p in [p.p50, p.p95, p.p99, p.p99_9] {
            assert_eq!(p.count, 100_000);
            assert_eq!(p.overflow, 1);
            assert_eq!(p.suppression, Some(Suppression::HistogramOverflow));
            assert_eq!(p.value_us, None);
            assert_eq!(p.ci95, None);
        }
    }

    #[test]
    fn counts_above_float_integer_precision_are_explicitly_suppressed() {
        let p = percentiles(&repeated((1_u64 << 53) + 1)).p99;
        assert_eq!(p.suppression, Some(Suppression::CountNotRepresentable));
        assert_eq!(p.value_us, None);
        assert_eq!(p.ci95, None);
    }

    #[test]
    fn merged_populations_gain_support_and_zero_latency_is_a_value() {
        let mut first = Distribution::default();
        let mut second = Distribution::default();
        first.hdr.record_n(0, 500).unwrap();
        second.hdr.record_n(0, 500).unwrap();
        assert_eq!(percentiles(&first).p99.value_us, None);
        first.merge(&second);
        let p = percentiles(&first).p99;
        assert_eq!(p.count, 1000);
        assert_eq!(p.value_us, Some(0));
        assert_eq!(p.ci95.unwrap().upper_us, 0);
    }
}
