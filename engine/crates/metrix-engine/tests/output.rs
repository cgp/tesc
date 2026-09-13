mod support;

use metrix_metrics::{Record, events::Phase};
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::{fs, path::Path, process::Stdio, time::Duration};
use tokio::{process::Command, sync::oneshot, time::timeout};

fn records(bytes: &[u8]) -> Vec<Record> {
    assert_eq!(bytes.last(), Some(&b'\n'));
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn time(record: &Record) -> u64 {
    match record {
        Record::RunStarted(r) => r.t_ms,
        Record::TargetStarted(r) => r.t_ms,
        Record::PhaseChanged(r) => r.t_ms,
        Record::TargetFinished(r) => r.t_ms,
        Record::Summary(r) => r.t_ms,
        Record::Request(r) => r.t_ms,
        Record::Annotation(r) => r.t_ms,
        Record::RunFinished(r) => r.t_ms,
    }
}

fn lifecycle(records: &[Record]) {
    assert!(
        matches!(records.first(), Some(Record::RunStarted(start)) if start.events_version == 1 && start.plan_hash.starts_with("sha256:") && start.plan_hash.len() == 71)
    );
    assert!(matches!(records.last(), Some(Record::RunFinished(end)) if end.exit_code == 0));
    assert!(
        records
            .windows(2)
            .all(|pair| time(&pair[0]) <= time(&pair[1]))
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TargetStarted(_)))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TargetFinished(_)))
            .count(),
        1
    );
    let phases: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            Record::PhaseChanged(r) => Some(r.phase),
            _ => None,
        })
        .collect();
    assert_eq!(phases, [Phase::Measure, Phase::Drain]);
}

/// Also records genuine Rust output for the cross-language schema check in check.sh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_conserve_counts_identify_requests_and_never_capture_secrets() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "calls/ping.json", |doc| {
        doc["ping"]["headers"] =
            json!({"Authorization": "Bearer PRIVATE_AUTH", "Cookie": "session=PRIVATE_COOKIE"});
        doc["ping"]["query"] = json!({"token": "PRIVATE_QUERY"});
        doc["ping"]["body"] = json!("PRIVATE_BODY");
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let events_path = dir.path().join("events.ndjson");
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--events")
        .arg(&events_path)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events_bytes = fs::read(&events_path).unwrap();
    for bytes in [&output.stdout, &events_bytes, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("PRIVATE_"));
    }
    let summaries = records(&output.stdout);
    let events = records(&events_bytes);
    lifecycle(&summaries);
    lifecycle(&events);
    assert_eq!(summaries.first(), events.first());
    assert!(!summaries.iter().any(|r| matches!(r, Record::Request(_))));
    assert!(!events.iter().any(|r| matches!(r, Record::Summary(_))));
    let requests: Vec<_> = events
        .iter()
        .filter_map(|r| match r {
            Record::Request(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(requests.len() >= 45);
    assert!(requests.iter().all(|r| r.target_id == "mock"
        && r.chain == "ping"
        && r.step == "get"
        && r.call == "ping"
        && r.status == Some(200)
        && r.error.is_none()
        && !r.sampled
        && r.ttfb_us.is_some()
        && r.bytes_received == 12
        && r.bytes_sent == 12
        && r.dns_us.is_none()
        && r.connect_us.is_none()
        && r.tls_us.is_none()));
    let mut iterations: Vec<_> = requests.iter().map(|r| r.iteration).collect();
    iterations.sort_unstable();
    assert_eq!(iterations, (0..requests.len() as u64).collect::<Vec<_>>());
    let completed: u64 = summaries
        .iter()
        .filter_map(|r| match r {
            Record::Summary(r) => Some(r.chains["ping"].iterations_completed),
            _ => None,
        })
        .sum();
    assert_eq!(completed, requests.len() as u64);
    let mut drift_samples = 0;
    let mut lag_samples = 0;
    let mut annotated_windows = 0;
    for pair in summaries.windows(2) {
        if let Record::Annotation(annotation) = &pair[0] {
            if annotation.code == "generator_self_metrics" {
                let Record::Summary(summary) = &pair[1] else {
                    panic!("missing companion summary");
                };
                assert_eq!(annotation.t_ms, summary.t_ms);
                let detail = annotation.detail.as_ref().unwrap();
                drift_samples += detail["drift_samples"].as_u64().unwrap();
                lag_samples += detail["scheduler_lag_samples"].as_u64().unwrap();
                annotated_windows += 1;
                assert!(summary.queue_depth <= summary.in_flight);
            }
        }
    }
    assert_eq!(drift_samples, requests.len() as u64);
    assert!(lag_samples > 0);
    assert_eq!(
        annotated_windows,
        summaries
            .iter()
            .filter(|r| matches!(r, Record::Summary(_)))
            .count()
    );
    let unavailable = summaries
        .iter()
        .find_map(|r| match r {
            Record::Annotation(r) if r.code == "self_metrics_unavailable" => r.detail.as_ref(),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        unavailable["unavailable"],
        json!(["cpu_pct", "rss_bytes", "open_fds"])
    );
    for record in &summaries {
        if let Record::Summary(summary) = record {
            let chain = &summary.chains["ping"];
            assert_eq!(chain.duration.count, chain.iterations_completed);
            assert_eq!(chain.steps["get"].total.count, chain.iterations_completed);
            assert_eq!(summary.bytes_received, chain.iterations_completed * 12);
            assert_eq!(summary.generator.events_dropped, 0);
        }
    }
    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-contract");
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join("summary.ndjson"), output.stdout).unwrap();
    fs::write(archive.join("events.ndjson"), events_bytes).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_is_deterministic_and_does_not_change_aggregate_counts() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(10);
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let mut selections = Vec::new();
    for index in 0..2 {
        let path = dir.path().join(format!("events-{index}.ndjson"));
        let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
            .arg("--plan")
            .arg(dir.path())
            .arg("--events")
            .arg(&path)
            .args(["--sample-rate", "0.5", "--seed", "42"])
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let events = records(&fs::read(path).unwrap());
        let selected: Vec<_> = events
            .iter()
            .filter_map(|r| match r {
                Record::Request(r) => {
                    assert!(r.sampled);
                    Some(r.iteration)
                }
                _ => None,
            })
            .collect();
        assert!(!selected.is_empty() && selected.len() < 10);
        let completed: u64 = records(&output.stdout)
            .iter()
            .filter_map(|r| match r {
                Record::Summary(r) => Some(r.chains["ping"].iterations_completed),
                _ => None,
            })
            .sum();
        assert_eq!(completed, 10);
        let summary = records(&output.stdout);
        let warning = summary
            .iter()
            .find_map(|r| match r {
                Record::Annotation(a) if a.code == "planned_sample_count_low" => a.detail.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(warning["planned_samples"], json!(10.0));
        let p = summary
            .iter()
            .find_map(|r| match r {
                Record::Annotation(a) if a.code == "load_percentiles" => a.detail.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(p["request_total"]["p50"]["count"], json!(10));
        assert_eq!(p["request_total"]["p50"]["support"], json!("suppressed"));
        assert!(p["request_total"]["p50"]["value_us"].is_null());
        assert!(String::from_utf8_lossy(&output.stderr).contains("events_dropped=0"));
        selections.push(selected);
    }
    assert_eq!(selections[0], selections[1]);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_events_pipe_cannot_stall_measurement_or_shutdown() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(1000);
        doc["load"]["duration"] = json!("3s");
        doc["load"]["max_concurrency"] = json!(100);
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let summary = dir.path().join("summary.ndjson");
    let mut child = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--summary")
        .arg(&summary)
        .args(["--events", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let unread = child.stdout.take().unwrap();
    let result = timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    drop(unread);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(result.status.code(), Some(1));
    let diagnostic = String::from_utf8(result.stderr).unwrap();
    assert!(diagnostic.contains("writers_unfinished=1"), "{diagnostic}");
    let summaries = records(&fs::read(summary).unwrap());
    let completed: u64 = summaries
        .iter()
        .filter_map(|r| match r {
            Record::Summary(r) => Some(r.chains["ping"].iterations_completed),
            _ => None,
        })
        .sum();
    assert!(completed > 1500, "{diagnostic}");
    assert!(
        summaries
            .iter()
            .any(|r| matches!(r, Record::Annotation(r) if r.code == "events_dropped"))
    );
    assert!(matches!(summaries.last(), Some(Record::RunFinished(end)) if end.exit_code == 1));
}

#[tokio::test]
async fn invalid_destinations_and_sampling_fail_before_traffic_and_do_not_overwrite_files() {
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    let file = dir.path().join("recording");
    fs::write(&file, "keep me").unwrap();
    for args in [
        vec!["--events", "-"],
        vec!["--sample-rate", "NaN"],
        vec!["--sample-rate", "1.1"],
        vec!["--summary", file.to_str().unwrap()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
            .arg("--plan")
            .arg(dir.path())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("target setup failed"));
    }
    assert_eq!(fs::read_to_string(file).unwrap(), "keep me");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_cancellation_and_setup_failure_have_terminal_records() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: 2000.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["defaults"]["timeout_ms"] = json!(20);
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let path = dir.path().join("timeout.ndjson");
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--events")
        .arg(&path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let events = records(&fs::read(path).unwrap());
    let failures: Vec<_> = events
        .iter()
        .filter_map(|r| match r {
            Record::Request(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(failures.len() >= 45);
    assert!(failures.iter().all(|r| {
        r.status.is_none()
            && r.ttfb_us.is_none()
            && r.error.as_ref().is_some_and(|e| {
                e.class == metrix_metrics::events::ErrorClass::Other
                    && e.message == "request deadline exceeded"
            })
    }));
    let total_failed: u64 = records(&output.stdout)
        .iter()
        .filter_map(|r| match r {
            Record::Summary(r) => Some(r.chains["ping"].steps["get"].failed),
            _ => None,
        })
        .sum();
    assert_eq!(total_failed, failures.len() as u64);

    support::edit(dir.path(), "mix.json", |doc| {
        doc["defaults"]["timeout_ms"] = json!(3000);
    });
    let plan = metrix_engine::Plan::load(dir.path()).unwrap();
    let summary = dir.path().join("cancel-summary.ndjson");
    let events = dir.path().join("cancel-events.ndjson");
    let sink = metrix_engine::Output::open(&plan, &summary, Some(&events), 1.0, 0).unwrap();
    let report =
        metrix_engine::run_with_output(plan, tokio::time::sleep(Duration::from_millis(100)), &sink)
            .await
            .unwrap();
    assert!(report.interrupted && report.cancelled > 0);
    assert!(!sink.finish(130, Some("interrupted".into())).failed);
    let events = records(&fs::read(events).unwrap());
    assert_eq!(events.iter().filter(|r| matches!(r, Record::Request(r) if r.error.as_ref().is_some_and(|e| e.message == "request cancelled"))).count() as u64, report.cancelled);
    assert!(matches!(events.last(), Some(Record::RunFinished(end)) if end.exit_code == 130));
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();

    support::edit(dir.path(), "targets.json", |doc| {
        doc["list"][0]["address"] = json!("127.0.0.1:1");
    });
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let events = records(&output.stdout);
    assert!(matches!(events.first(), Some(Record::RunStarted(_))));
    assert!(
        matches!(events.last(), Some(Record::RunFinished(end)) if end.exit_code == 1 && end.stopped_because.is_some())
    );
}
