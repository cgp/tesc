use metrix_mock::{Config, Latency};
use serde_json::json;

#[test]
fn cli_explains_usage_and_rejects_missing_or_unknown_arguments() {
    let binary = env!("CARGO_BIN_EXE_metrix-mock");
    let help = std::process::Command::new(binary)
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--config") && help.contains("--listen"));
    for args in [
        ["--config", "--listen"],
        ["--listen", "invalid"],
        ["--unknown", "1"],
    ] {
        let output = std::process::Command::new(binary)
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn example_and_defaults_validate() {
    Config::default().validate().unwrap();
    let example: Config =
        serde_json::from_str(include_str!("../../../../examples/mock.json")).unwrap();
    example.validate().unwrap();
}

#[test]
fn rejects_bad_configuration_before_serving() {
    for value in [
        json!({"latency": {"type": "fixed", "ms": -1}}),
        json!({"latency": {"type": "normal", "mean_ms": 10, "stddev_ms": -1}}),
        json!({"latency": {"type": "lognormal", "median_ms": 0, "sigma": 1}}),
        json!({"latency": {"type": "lognormal", "median_ms": 1, "sigma": -1}}),
        json!({"latency": {"type": "bimodal", "fast_ms": 20, "slow_ms": 10, "slow_probability": 0.5}}),
        json!({"latency": {"type": "bimodal", "fast_ms": 0, "slow_ms": 10, "slow_probability": 2}}),
        json!({"max_latency_ms": 3_600_001}),
        json!({"errors": [{"type": "http", "rate": 1, "status": 200}]}),
        json!({"errors": [{"type": "http", "rate": 1, "status": 600}]}),
        json!({"errors": [{"type": "disconnect", "rate": -0.1}]}),
        json!({"errors": [{"type": "disconnect", "rate": 0.6}, {"type": "timeout", "rate": 0.5, "delay_ms": 10}]}),
        json!({"errors": [{"type": "timeout", "rate": 1, "delay_ms": 0}]}),
        json!({"slow_start": {"duration_ms": 0, "extra_latency_ms": 10}}),
        json!({"slow_start": {"duration_ms": 1, "extra_latency_ms": -10}}),
        json!({"capacity_rps": 0}),
        json!({"max_in_flight": 0}),
        json!({"max_connections": 0}),
    ] {
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert!(config.validate().is_err(), "accepted {value}");
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            Config {
                latency: Latency::Fixed { ms: value },
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn rejects_typos_missing_parameters_and_unknown_types() {
    for value in [
        json!({"capacity": 100}),
        json!({"latency": {"type": "fixed", "ms": 10, "typo": 1}}),
        json!({"latency": {"type": "normal", "mean_ms": 10}}),
        json!({"latency": {"type": "exponential", "ms": 10}}),
        json!({"errors": [{"type": "disconnect", "rate": 1, "status": 500}]}),
        json!({"slow_start": {"duration_ms": 1, "extra_latency_ms": 1, "typo": 1}}),
    ] {
        assert!(
            serde_json::from_value::<Config>(value.clone()).is_err(),
            "accepted {value}"
        );
    }
}
