mod support;
use metrix_engine::{Plan, run};
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::future::pending;

#[tokio::test]
async fn known_target_ceiling_is_bracketed_within_one_step() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            capacity_rps: Some(100),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "calls/ping.json", |d| {
        d["ping"]["assert"] = json!([{"status":200}])
    });
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["max_concurrency"] = json!(128);
        d["engine"]["connections_per_host"] = json!(128);
        d["engine"]["send_drift_threshold_ms"] = json!(100);
        d["engine"]["rate_tolerance_pct"] = json!(20);
        d["load"]["breakpoint"] = json!({"start_rate":75,"step_rate":75,"max_rate":300,"step_duration":"4s","step_recovery":"1s","stop_on":{"error_rate":0.02}});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let report = run(Plan::load(dir.path()).unwrap(), pending())
        .await
        .unwrap();
    assert!(!report.generator_limited);
    let search = report.breakpoint.unwrap();
    assert_eq!(search.stopped_because, "error_rate");
    assert_eq!(search.steps.len(), 2);
    assert_eq!(search.steps[0].rate, 75.0);
    assert_eq!(search.steps[1].rate, 150.0);
    assert!(search.steps[1].measured_seconds < 4.0);
    task.abort();
}

#[tokio::test]
async fn an_underprovisioned_generator_aborts_instead_of_claiming_a_target_limit() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: 500.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["max_concurrency"] = json!(1);
        d["load"]["breakpoint"] = json!({"start_rate":50,"step_rate":50,"max_rate":200,"step_duration":"4s","stop_on":{"rate_shortfall_pct":10}});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let report = run(Plan::load(dir.path()).unwrap(), pending())
        .await
        .unwrap();
    assert!(report.generator_limited);
    assert_eq!(
        report.breakpoint.unwrap().stopped_because,
        "generator_limited"
    );
    task.abort();
}

#[tokio::test]
async fn unsupported_latency_threshold_never_finds_a_breakpoint() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["breakpoint"] = json!({"start_rate":10,"step_rate":10,"max_rate":20,"step_duration":"1s","stop_on":{"p99_latency_ms":1}});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let report = run(Plan::load(dir.path()).unwrap(), pending())
        .await
        .unwrap();
    assert_eq!(
        report.breakpoint.unwrap().stopped_because,
        "insufficient_samples"
    );
    task.abort();
}
