mod support;
use metrix_engine::{Output, Plan, run_with_output};
use metrix_metrics::{Phase, Record, Severity};
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::{fs, future::pending, time::Duration};

#[tokio::test]
async fn healthy_load_does_not_claim_a_rate_cap_or_drift_failure() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |d| {
        d["engine"]["send_drift_threshold_ms"] = json!(1000);
    });
    let server = tokio::spawn(server.run_until(pending()));
    let path = dir.path().join("summary.ndjson");
    let plan = Plan::load(dir.path()).unwrap();
    let output = Output::open(&plan, &path, None, 0.0, 0).unwrap();
    let report = run_with_output(plan, pending(), &output).await.unwrap();
    assert!(!output.finish(0, None).failed);
    server.abort();
    assert!(report.sent >= 49);
    for line in fs::read_to_string(path).unwrap().lines() {
        let r: Record = serde_json::from_str(line).unwrap();
        if let Record::Annotation(a) = r {
            assert!(!matches!(
                a.code.as_str(),
                "rate_not_achieved" | "concurrency_cap_reached" | "send_schedule_drift"
            ));
        }
    }
}

#[tokio::test]
async fn concurrency_and_connection_shortfalls_are_distinct_and_draining_is_not_load_time() {
    for (protocol, concurrency, connections, capped) in
        [("http2", 1, 1, true), ("http1", 20, 1, false)]
    {
        let server = MockServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            Config {
                latency: Latency::Fixed { ms: 300.0 },
                ..Config::default()
            },
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), server.local_addr().unwrap(), protocol);
        support::edit(dir.path(), "mix.json", |d| {
            d["load"]["rate"] = json!(20);
            d["load"]["max_concurrency"] = json!(concurrency);
            d["engine"]["connections_per_host"] = json!(connections);
            d["engine"]["send_drift_threshold_ms"] = json!(1000);
        });
        let server = tokio::spawn(server.run_until(pending()));
        let path = dir.path().join("summary.ndjson");
        let plan = Plan::load(dir.path()).unwrap();
        let output = Output::open(&plan, &path, None, 0.0, 0).unwrap();
        let report = run_with_output(plan, pending(), &output).await.unwrap();
        assert!(!output.finish(0, None).failed);
        server.abort();
        let records: Vec<Record> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let annotations: Vec<_> = records
            .iter()
            .filter_map(|r| {
                if let Record::Annotation(a) = r {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            annotations
                .iter()
                .all(|a| a.to_ms.is_none_or(|to| to >= a.from_ms && to <= a.t_ms))
        );
        let rate = annotations
            .iter()
            .rfind(|a| a.code == "rate_not_achieved")
            .unwrap();
        assert_eq!(rate.phase, Some(Phase::Measure));
        assert_eq!(rate.detail.as_ref().unwrap()["offered"], json!(20));
        assert_eq!(rate.detail.as_ref().unwrap()["sends"], json!(report.sent));
        assert_eq!(rate.detail.as_ref().unwrap()["window_ms"], json!(1000));
        let cap = annotations
            .iter()
            .rfind(|a| a.code == "concurrency_cap_reached");
        if capped {
            let cap = cap.unwrap();
            assert_eq!(cap.severity, Severity::Invalid);
            assert!(
                cap.detail.as_ref().unwrap()["duration_ms"]
                    .as_u64()
                    .unwrap()
                    > 250
            );
            assert!(report.skipped_concurrency > 0);
            assert_eq!(report.skipped_connections, 0);
        } else {
            assert!(cap.is_none());
            assert_eq!(report.skipped_concurrency, 0);
            assert!(report.skipped_connections > 0);
            assert_eq!(report.diagnostics.measure.cap_duration, Duration::ZERO);
        }
        assert!(!annotations.iter().any(|a| a.code == "send_schedule_drift"));
        let low = annotations
            .iter()
            .find(|a| a.code == "sample_count_low")
            .unwrap();
        assert_eq!(low.severity, Severity::Warn);
        assert_eq!(low.detail.as_ref().unwrap()["partial"], json!(false));
        assert!(
            low.detail.as_ref().unwrap()["suppressed"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["count"].as_u64().unwrap() <= report.responses)
        );
    }
}
