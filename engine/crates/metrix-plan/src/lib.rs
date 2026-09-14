//! Call, mix, and targets types; validation; schema generation.
//!
//! The plan is **three documents**, kept separate because they are authored by
//! different parties, change at different rates, and are reused differently:
//!
//! | Document | Contains | Changes when |
//! |---|---|---|
//! | [`call::Call`] | Individual request definitions | The service's API changes |
//! | [`mix::Mix`] | Named chains of calls and their share of the load | The question changes |
//! | [`targets::Targets`] | The boxes to run against | The environment changes |
//!
//! Together they form a bundle directory, which is the unit of execution and of
//! transfer. See `docs/design-engine.md` §4.
//!
//! These types are authoritative: the JSON Schemas the API validates against are
//! generated from them (F0.3), so the two sides cannot drift.

pub mod auth;
pub mod call;
pub mod common;
pub mod mix;
pub mod targets;

pub use call::{Assertion, Call, CallFile, Condition, Generate};
pub use common::{Dur, Method, Selector};
pub use mix::{
    Chain, Corpus, Dataset, DatasetMode, Defaults, ExecProtocol, Generator, Load, LoadMode,
    LoadModel, Mix, OnFailure, Phases, RepeatUntil, SessionPolicy, Step,
};
pub use targets::{Target, Targets};

/// The statistical floor: 30s at 75 RPS. Supports p50 and p95 solidly, p99 coarsely
/// (95% CI spans p98.6–p99.4), and does not support p99.9 at all. Validation warns
/// below this; see `docs/design-engine.md` §12.1.
pub const MIN_SAMPLES: u64 = 2250;

/// Chain percentages must total this, within [`PERCENT_EPSILON`].
pub const PERCENT_TOTAL: f64 = 100.0;

/// Tolerance for the percentage total, so `16.67 × 6` is not a validation failure.
pub const PERCENT_EPSILON: f64 = 0.01;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_trip_in_the_spelling_they_were_written() {
        for (input, secs) in [("30s", 30), ("10m", 600), ("1h", 3600), ("1h30m", 5400)] {
            let d: Dur = input.parse().unwrap();
            assert_eq!(d.as_duration().as_secs(), secs, "parsing {input}");
        }
        // Display picks the largest unit that divides evenly.
        assert_eq!(Dur::from_secs(90).to_string(), "90s");
        assert_eq!(Dur::from_secs(120).to_string(), "2m");
    }

    #[test]
    fn durations_without_units_are_rejected_with_a_useful_message() {
        let err = "30".parse::<Dur>().unwrap_err();
        assert!(err.to_string().contains("write 30s, not 30"), "{err}");
    }

    #[test]
    fn phases_default_to_on() {
        let p = Phases::default();
        assert_eq!(p.baseline, Dur::from_secs(30));
        assert_eq!(p.settle, Dur::from_secs(60));
    }

    #[test]
    fn a_condition_with_nothing_in_it_is_detectable() {
        let c: Condition = serde_json::from_str("{}").unwrap();
        assert!(c.is_empty());
    }
}
