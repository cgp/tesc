mod support;
use metrix_engine::{Output, Plan, run_with_output};
use metrix_mock::{Config, MockServer};
use serde_json::json;
use std::{fs, future::pending};

#[tokio::test]
async fn each_step_has_independent_samples_and_observed_recovery() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["breakpoint"] = json!({"start_rate": 5, "step_rate": 5, "max_rate": 15, "step_duration": "1s", "step_recovery": "1s"});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let plan = Plan::load(dir.path()).unwrap();
    let path = dir.path().join("out.ndjson");
    let output = Output::open(&plan, &path, None, 1.0, 0).unwrap();
    let report = run_with_output(plan, pending(), &output).await.unwrap();
    assert!(!output.finish(0, None).failed);
    let search = report.breakpoint.unwrap();
    assert_eq!(
        search.steps.iter().map(|s| s.rate).collect::<Vec<_>>(),
        [5.0, 10.0, 15.0]
    );
    for step in &search.steps {
        assert_eq!(step.p99.count, step.requests);
        assert!(step.p99.value_us.is_none());
    }
    let records: Vec<serde_json::Value> = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|r| r["code"] == "load_percentiles")
            .count(),
        3
    );
    assert!(records.iter().any(|r| r["type"] == "summary"
        && r["phase"] == "settle"
        && r["target_rate"].as_f64() == Some(0.0)));
    task.abort();
}

#[test]
fn invalid_ramps_fail_before_network_setup() {
    for patch in [
        json!({"step_rate":0}),
        json!({"step_factor":1}),
        json!({"max_rate":1}),
        json!({"step_duration":"0s"}),
    ] {
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        support::edit(dir.path(), "mix.json", |d| {
            d["load"]["mode"] = json!("breakpoint");
            let mut b = json!({"start_rate":5, "step_rate":5, "max_rate":15, "step_duration":"1s"});
            for (k, v) in patch.as_object().unwrap() {
                b[k] = v.clone();
            }
            d["load"]["breakpoint"] = b;
        });
        assert!(Plan::load(dir.path()).is_err());
    }
}
