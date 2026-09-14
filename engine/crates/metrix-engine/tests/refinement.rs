mod support;
use metrix_engine::{Plan, run};
use metrix_mock::{Config, MockServer};
use serde_json::json;
use std::future::pending;

#[test]
fn impossible_recovery_and_sweep_gaps_are_refused_before_traffic() {
    for sweep in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        if sweep {
            support::edit(dir.path(), "targets.json", |d| {
                d["gap"] = json!("18446744073709551615s");
                d["list"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":"second","address":"127.0.0.1:2"}));
            });
        } else {
            support::edit(dir.path(), "mix.json", |d| {
                d["load"]["mode"] = json!("breakpoint");
                d["load"]["breakpoint"] = json!({"start_rate":5,"step_rate":5,"max_rate":15,"step_duration":"1s","step_recovery":"18446744073709551615s","refine":true});
            });
        }
        assert!(Plan::load(dir.path()).is_err());
    }
}

#[tokio::test]
async fn refinement_runs_one_full_step_and_reports_a_bracket() {
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
        d["load"]["breakpoint"] = json!({"start_rate":75,"step_rate":75,"max_rate":300,"step_duration":"4s","step_recovery":"1s","refine":true,"stop_on":{"error_rate":0.02}});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let report = run(Plan::load(dir.path()).unwrap(), pending())
        .await
        .unwrap();
    assert!(!report.generator_limited);
    let search = report.breakpoint.unwrap();
    assert_eq!(search.steps.iter().filter(|s| s.refinement).count(), 1);
    let refined = search.steps.last().unwrap();
    assert_eq!(refined.rate, 112.5);
    assert!(refined.measured_seconds >= 4.0);
    let bracket = search.bracket.unwrap();
    assert!(bracket[1] - bracket[0] <= 37.5);
    assert_eq!(search.cliff, Some(150.0));
    assert!(search.max_sustained_rate.is_some());
    assert!(search.limiting_resource.is_none());
    assert!(search.resource_attribution.contains("observation"));
    assert!(search.steps.iter().any(|s| s.statuses.contains_key("503")));
    task.abort();
}
