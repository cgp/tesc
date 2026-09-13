mod support;

use metrix_mock::{Config, MockServer};
use serde_json::json;
use std::{collections::HashMap, fs, time::Duration};
use tokio::{
    process::Command,
    sync::oneshot,
    time::{Instant, timeout},
};

/// Exercised by scripts/check.sh engine: the copied binary needs only its bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_binary_holds_75_rps_for_30_seconds_without_the_api() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("plan");
    support::bundle(&plan, server.local_addr().unwrap(), "http1");
    support::edit(&plan, "mix.json", |doc| {
        doc["load"]["rate"] = json!(75);
        doc["load"]["duration"] = json!("30s");
    });
    let binary = dir
        .path()
        .join(format!("metrix-engine{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_metrix-engine"), &binary).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let start = Instant::now();
    let output = timeout(
        Duration::from_secs(40),
        Command::new(&binary)
            .current_dir(dir.path())
            .arg("--plan")
            .arg("plan")
            .env_remove("PYTHONPATH")
            .env_remove("METRIX_HOME")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let elapsed = start.elapsed();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<metrix_metrics::Record> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(matches!(
        records.first(),
        Some(metrix_metrics::Record::RunStarted(_))
    ));
    assert!(
        matches!(records.last(), Some(metrix_metrics::Record::RunFinished(finished)) if finished.exit_code == 0)
    );
    let completed: u64 = records
        .iter()
        .filter_map(|record| match record {
            metrix_metrics::Record::Summary(summary) => {
                Some(summary.chains["ping"].iterations_completed)
            }
            _ => None,
        })
        .sum();
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    let values: HashMap<_, _> = diagnostic
        .split_whitespace()
        .filter_map(|word| word.split_once('='))
        .collect();
    let number = |key| values[key].parse::<u64>().unwrap();
    assert_eq!(number("offered"), 2250);
    assert_eq!(number("failed"), 0);
    assert_eq!(number("responses"), number("admitted"));
    assert_eq!(completed, number("responses"));
    assert_eq!(number("summaries_dropped"), 0);
    assert_eq!(number("sent_finished"), number("responses"));
    // A loaded CI host may skip late arrivals; require >=98% of the requested rate,
    // with the full shortfall and drift explicitly reported instead of hidden.
    assert!(number("responses") >= 2205, "{diagnostic}");
    assert_eq!(number("skipped_concurrency"), 0);
    assert_eq!(number("skipped_connections"), 0);
    assert!(values.contains_key("max_send_drift_ms"));
    assert_eq!(number("drift_samples"), number("sent_finished"));
    assert_eq!(number("sent"), number("sent_finished"));
    assert!(!records.iter().any(|r| matches!(r,
        metrix_metrics::Record::Annotation(a) if a.code == "planned_sample_count_low"
    )));
    let detail = records
        .iter()
        .find_map(|r| match r {
            metrix_metrics::Record::Annotation(a) if a.code == "load_percentiles" => {
                a.detail.as_ref()
            }
            _ => None,
        })
        .expect("standalone measured percentiles");
    assert_eq!(detail["partial"], json!(false));
    for distribution in ["chain_duration", "request_total", "ttfb"] {
        let p = &detail[distribution];
        assert_eq!(p["p99"]["count"], json!(completed));
        assert_eq!(p["p50"]["support"], json!("stable"));
        assert_eq!(p["p95"]["support"], json!("stable"));
        assert_eq!(p["p99"]["support"], json!("crude"));
        assert_eq!(p["p99_9"]["support"], json!("suppressed"));
        assert!(p["p99_9"]["value_us"].is_null());
        assert!(p["p99_9"]["ci95"].is_null());
        let value = p["p99"]["value_us"].as_u64().unwrap();
        let ci = &p["p99"]["ci95"];
        assert!(ci["lower_us"].as_u64().unwrap() <= value);
        assert!(ci["upper_us"].as_u64().unwrap() >= value);
        assert!(ci["lower_us"].as_u64().unwrap() >= 9900);
        let corrected = &detail["schedule_corrected"][distribution];
        assert_eq!(corrected["p99"]["count"], p["p99"]["count"]);
        assert_eq!(corrected["p99_9"]["support"], json!("suppressed"));
        assert!(corrected["p99"]["value_us"].as_u64().unwrap() >= value);
    }
    assert!(number("scheduler_lag_samples") > 0);
    assert!(values.contains_key("max_scheduler_lag_ms"));
    assert!(elapsed >= Duration::from_secs(30) && elapsed < Duration::from_secs(35));
}
