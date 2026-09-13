use std::time::Duration;

use rand::{Rng, SeedableRng, rngs::StdRng};
use tokio::time::Instant;

use crate::{Config, ErrorInjection};

#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    Http(u16),
    Disconnect,
    Timeout(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Latency, SlowStart};

    fn model(config: Config) -> Model {
        config.validate().unwrap();
        Model::new(config, Instant::now())
    }

    #[test]
    fn seeded_distributions_match_their_known_population() {
        // Means/variances and mixture mass exercise the actual sampled population,
        // not a second implementation of the sampler. No wall-clock timers involved.
        for (latency, expected_mean, expected_variance) in [
            (Latency::Fixed { ms: 12.0 }, 12.0, 0.0),
            (
                Latency::Normal {
                    mean_ms: 100.0,
                    stddev_ms: 10.0,
                },
                100.0,
                100.0,
            ),
            (
                Latency::Lognormal {
                    median_ms: 10.0,
                    sigma: 0.5,
                },
                10.0 * 0.125_f64.exp(),
                100.0 * (0.25_f64.exp() - 1.0) * 0.25_f64.exp(),
            ),
            (
                Latency::Bimodal {
                    fast_ms: 10.0,
                    slow_ms: 50.0,
                    slow_probability: 0.25,
                },
                20.0,
                300.0,
            ),
        ] {
            let config = Config {
                seed: 42,
                latency,
                ..Config::default()
            };
            let mut left = model(config.clone());
            let mut right = model(config);
            let mut sum = 0.0;
            let mut sum_squares = 0.0;
            let n = 100_000;
            for _ in 0..n {
                let decision = left.decide(left.start).unwrap();
                assert_eq!(
                    Some(Decision {
                        delay: decision.delay,
                        outcome: Outcome::Http(200)
                    }),
                    right.decide(right.start)
                );
                let ms = decision.delay.as_secs_f64() * 1000.0;
                sum += ms;
                sum_squares += ms * ms;
            }
            let mean = sum / f64::from(n);
            let variance = sum_squares / f64::from(n) - mean * mean;
            assert!(
                (mean - expected_mean).abs() < 0.2,
                "mean {mean} expected {expected_mean}, n={n}"
            );
            assert!(
                (variance - expected_variance).abs() < expected_variance * 0.03 + 1e-8,
                "variance {variance} expected {expected_variance}, n={n}"
            );
        }
    }

    #[test]
    fn clipping_handles_negative_normal_and_overflowing_lognormal_samples() {
        for latency in [
            Latency::Normal {
                mean_ms: 0.0,
                stddev_ms: 100.0,
            },
            Latency::Lognormal {
                median_ms: 1.0,
                sigma: 100.0,
            },
        ] {
            let mut model = model(Config {
                latency,
                max_latency_ms: 20.0,
                ..Config::default()
            });
            let mut zeros = 0;
            let mut capped = 0;
            for _ in 0..10_000 {
                let delay = model.decide(model.start).unwrap().delay;
                assert!(delay <= Duration::from_millis(20));
                zeros += usize::from(delay.is_zero());
                capped += usize::from(delay == Duration::from_millis(20));
            }
            assert!(zeros > 100 && capped > 100);
        }
    }

    #[test]
    fn slow_start_decays_by_elapsed_time_and_caps_total_delay() {
        let mut model = model(Config {
            latency: Latency::Fixed { ms: 10.0 },
            slow_start: Some(SlowStart {
                duration_ms: 1000.0,
                extra_latency_ms: 100.0,
            }),
            max_latency_ms: 100.0,
            ..Config::default()
        });
        for (elapsed, expected) in [(0, 100), (500, 60), (1000, 10), (2000, 10)] {
            assert_eq!(
                model
                    .decide(model.start + Duration::from_millis(elapsed))
                    .unwrap()
                    .delay,
                Duration::from_millis(expected)
            );
        }
    }

    #[test]
    fn capacity_allows_one_second_burst_refills_and_does_not_queue() {
        let config = Config {
            capacity_rps: Some(2),
            ..Config::default()
        };
        let mut model = model(config);
        let start = model.start;
        assert!(model.decide(start).is_some());
        assert!(model.decide(start).is_some());
        assert!(model.decide(start).is_none());
        assert!(model.decide(start + Duration::from_millis(499)).is_none());
        assert!(model.decide(start + Duration::from_millis(500)).is_some());
        let later = start + Duration::from_secs(100);
        assert!(model.decide(later).is_some());
        assert!(model.decide(later).is_some());
        assert!(model.decide(later).is_none());
    }

    #[test]
    fn rejections_preserve_random_sequence() {
        let config = Config {
            latency: Latency::Normal {
                mean_ms: 10.0,
                stddev_ms: 2.0,
            },
            ..Config::default()
        };
        let mut unlimited = model(config.clone());
        let mut limited = model(Config {
            capacity_rps: Some(1),
            ..config
        });
        for n in 0..100 {
            assert_eq!(
                unlimited.decide(unlimited.start),
                limited.decide(limited.start + Duration::from_secs(n))
            );
            assert!(
                limited
                    .decide(limited.start + Duration::from_secs(n))
                    .is_none()
            );
        }
    }

    #[test]
    fn error_rates_are_unconditional_and_leave_the_remaining_successes() {
        let mut model = model(Config {
            errors: vec![
                ErrorInjection::Http {
                    rate: 0.1,
                    status: 429,
                },
                ErrorInjection::Disconnect { rate: 0.2 },
                ErrorInjection::Timeout {
                    rate: 0.3,
                    delay_ms: 5000.0,
                },
            ],
            ..Config::default()
        });
        let mut counts = [0_i32; 4];
        for _ in 0..100_000 {
            counts[match model.decide(model.start).unwrap().outcome {
                Outcome::Http(429) => 0,
                Outcome::Disconnect => 1,
                Outcome::Timeout(duration) => {
                    assert_eq!(duration, Duration::from_secs(5));
                    2
                }
                Outcome::Http(200) => 3,
                other => panic!("unexpected {other:?}"),
            }] += 1;
        }
        for (count, expected) in counts.into_iter().zip([10_000, 20_000, 30_000, 40_000]) {
            assert!(
                (count - expected).abs() < 500_i32,
                "count={count}, expected={expected}, n=100000"
            );
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Decision {
    pub delay: Duration,
    pub outcome: Outcome,
}

pub(crate) struct Model {
    config: Config,
    rng: StdRng,
    start: Instant,
    last_refill: Instant,
    tokens: f64,
}

impl Model {
    pub fn new(config: Config, now: Instant) -> Self {
        Self {
            rng: StdRng::seed_from_u64(config.seed),
            tokens: f64::from(config.capacity_rps.unwrap_or(0)),
            config,
            start: now,
            last_refill: now,
        }
    }

    pub fn decide(&mut self, now: Instant) -> Option<Decision> {
        if let Some(rate) = self.config.capacity_rps {
            let rate = f64::from(rate);
            self.tokens =
                (self.tokens + now.duration_since(self.last_refill).as_secs_f64() * rate).min(rate);
            self.last_refill = now;
            if self.tokens < 1.0 {
                return None;
            }
            self.tokens -= 1.0;
        }
        let sample = self.config.latency.sample(&mut self.rng).max(0.0);
        let extra = self.config.slow_start.as_ref().map_or(0.0, |slow| {
            let elapsed_ms = now.duration_since(self.start).as_secs_f64() * 1000.0;
            slow.extra_latency_ms * (1.0 - elapsed_ms / slow.duration_ms).clamp(0.0, 1.0)
        });
        let delay =
            Duration::from_secs_f64((sample + extra).min(self.config.max_latency_ms) / 1000.0);
        let mut roll = self.rng.random::<f64>();
        let mut outcome = Outcome::Http(200);
        for error in &self.config.errors {
            if roll < error.rate() {
                outcome = match *error {
                    ErrorInjection::Http { status, .. } => Outcome::Http(status),
                    ErrorInjection::Disconnect { .. } => Outcome::Disconnect,
                    ErrorInjection::Timeout { delay_ms, .. } => {
                        Outcome::Timeout(Duration::from_secs_f64(delay_ms / 1000.0))
                    }
                };
                break;
            }
            roll -= error.rate();
        }
        Some(Decision { delay, outcome })
    }
}
