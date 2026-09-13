use std::io;

use rand::Rng;
use rand_distr::{Distribution, StandardNormal};
use serde::Deserialize;

/// Mock configuration, separate from the engine's plan format.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub seed: u64,
    pub latency: Latency,
    pub max_latency_ms: f64,
    pub errors: Vec<ErrorInjection>,
    pub slow_start: Option<SlowStart>,
    pub capacity_rps: Option<u32>,
    pub max_in_flight: Option<usize>,
    pub max_connections: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: 0,
            latency: Latency::Fixed { ms: 10.0 },
            max_latency_ms: 60_000.0,
            errors: Vec::new(),
            slow_start: None,
            capacity_rps: None,
            max_in_flight: None,
            max_connections: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Latency {
    Fixed {
        ms: f64,
    },
    Normal {
        mean_ms: f64,
        stddev_ms: f64,
    },
    Lognormal {
        median_ms: f64,
        sigma: f64,
    },
    Bimodal {
        fast_ms: f64,
        slow_ms: f64,
        slow_probability: f64,
    },
}

impl Latency {
    pub(crate) fn sample(&self, rng: &mut impl Rng) -> f64 {
        match *self {
            Self::Fixed { ms } => ms,
            Self::Normal { mean_ms, stddev_ms } => {
                let z: f64 = StandardNormal.sample(rng);
                mean_ms + stddev_ms * z
            }
            Self::Lognormal { median_ms, sigma } => {
                let z: f64 = StandardNormal.sample(rng);
                (median_ms.ln() + sigma * z).exp()
            }
            Self::Bimodal {
                fast_ms,
                slow_ms,
                slow_probability,
            } => {
                if rng.random::<f64>() < slow_probability {
                    slow_ms
                } else {
                    fast_ms
                }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ErrorInjection {
    Http { rate: f64, status: u16 },
    Disconnect { rate: f64 },
    Timeout { rate: f64, delay_ms: f64 },
}

impl ErrorInjection {
    pub(crate) fn rate(&self) -> f64 {
        match *self {
            Self::Http { rate, .. } | Self::Disconnect { rate } | Self::Timeout { rate, .. } => {
                rate
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowStart {
    pub duration_ms: f64,
    pub extra_latency_ms: f64,
}

impl Config {
    /// Reject invalid parameters before opening a listener or constructing timers.
    pub fn validate(&self) -> io::Result<()> {
        use Latency::*;
        match self.latency {
            Fixed { ms } => milliseconds("latency.ms", ms)?,
            Normal { mean_ms, stddev_ms } => {
                milliseconds("latency.mean_ms", mean_ms)?;
                milliseconds("latency.stddev_ms", stddev_ms)?;
            }
            Lognormal { median_ms, sigma } => {
                milliseconds("latency.median_ms", median_ms)?;
                require(median_ms > 0.0, "latency.median_ms must be positive")?;
                require(
                    sigma.is_finite() && (0.0..=100.0).contains(&sigma),
                    "latency.sigma must be finite and between 0 and 100",
                )?;
            }
            Bimodal {
                fast_ms,
                slow_ms,
                slow_probability,
            } => {
                milliseconds("latency.fast_ms", fast_ms)?;
                milliseconds("latency.slow_ms", slow_ms)?;
                require(slow_ms >= fast_ms, "latency.slow_ms must be >= fast_ms")?;
                probability("latency.slow_probability", slow_probability)?;
            }
        }
        milliseconds("max_latency_ms", self.max_latency_ms)?;
        let mut total = 0.0;
        for (index, error) in self.errors.iter().enumerate() {
            probability(&format!("errors[{index}].rate"), error.rate())?;
            total += error.rate();
            match *error {
                ErrorInjection::Http { status, .. } => require(
                    (400..=599).contains(&status),
                    &format!("errors[{index}].status must be between 400 and 599"),
                )?,
                ErrorInjection::Timeout { delay_ms, .. } => {
                    milliseconds(&format!("errors[{index}].delay_ms"), delay_ms)?;
                    require(
                        delay_ms > 0.0,
                        &format!("errors[{index}].delay_ms must be positive"),
                    )?;
                }
                ErrorInjection::Disconnect { .. } => {}
            }
        }
        require(total <= 1.0, "errors rates must sum to at most 1")?;
        if let Some(slow) = &self.slow_start {
            milliseconds("slow_start.duration_ms", slow.duration_ms)?;
            milliseconds("slow_start.extra_latency_ms", slow.extra_latency_ms)?;
            require(
                slow.duration_ms > 0.0,
                "slow_start.duration_ms must be positive",
            )?;
        }
        require(
            self.capacity_rps != Some(0),
            "capacity_rps must be positive",
        )?;
        for (name, limit) in [
            ("max_in_flight", self.max_in_flight),
            ("max_connections", self.max_connections),
        ] {
            if let Some(limit) = limit {
                require(
                    (1..=tokio::sync::Semaphore::MAX_PERMITS).contains(&limit),
                    &format!(
                        "{name} must be positive and <= {}",
                        tokio::sync::Semaphore::MAX_PERMITS
                    ),
                )?;
            }
        }
        Ok(())
    }
}

fn milliseconds(name: &str, value: f64) -> io::Result<()> {
    require(
        value.is_finite() && (0.0..=3_600_000.0).contains(&value),
        &format!("{name} must be finite and between 0 and 3600000 milliseconds"),
    )
}

fn probability(name: &str, value: f64) -> io::Result<()> {
    require(
        value.is_finite() && (0.0..=1.0).contains(&value),
        &format!("{name} must be finite and between 0 and 1"),
    )
}

fn require(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, message))
    }
}
