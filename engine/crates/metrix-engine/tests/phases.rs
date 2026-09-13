mod support;
use metrix_engine::{Output, Plan, run_with_output};
use metrix_metrics::{Record, events::Phase};
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::{fs, future::pending, path::Path, time::Duration};
use tokio::time::{Instant, timeout};

fn records(path: &Path) -> Vec<Record> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn transitions(records: &[Record]) -> Vec<(Phase, u64)> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::PhaseChanged(r) => Some((r.phase, r.t_ms)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn drain_keeps_original_deadlines_and_warmup_failures_out_of_measured_totals() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: 5000.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["warmup"] = json!("1s");
        doc["load"]["duration"] = json!("1s");
        doc["load"]["rate"] = json!(2);
        doc["defaults"]["timeout_ms"] = json!(700);
    });
    let server = tokio::spawn(server.run_until(pending()));
    let summary = dir.path().join("summary.ndjson");
    let events = dir.path().join("events.ndjson");
    let plan = Plan::load(dir.path()).unwrap();
    let sink = Output::open(&plan, &summary, Some(&events), 1.0, 0).unwrap();
    let report = timeout(
        Duration::from_secs(5),
        run_with_output(plan, pending(), &sink),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!sink.finish(0, None).failed);
    server.abort();
    assert_eq!(report.timed_out, 4);
    assert_eq!(report.metrics.counters.failed, 2);
    assert_eq!(report.warmup_metrics.counters.failed, 2);
    assert_eq!(report.metrics.total.count(), 2);
    assert_eq!(report.warmup_metrics.total.count(), 2);
    let events = records(&events);
    let requests: Vec<_> = events
        .iter()
        .filter_map(|r| match r {
            Record::Request(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|r| r.error.is_some() && r.total_us >= 650_000 && r.total_us < 850_000)
    );
    assert_eq!(
        requests.iter().filter(|r| r.phase == Phase::Warmup).count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.phase == Phase::Measure)
            .count(),
        2
    );
}

#[tokio::test]
async fn all_phases_preserve_warmup_identity_and_exclude_late_samples_from_measure() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: 2500.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["phases"] = json!({"baseline": "1s", "settle": "1s"});
        doc["load"]["warmup"] = json!("1s");
        doc["load"]["duration"] = json!("1s");
        doc["load"]["rate"] = json!(2);
        doc["defaults"]["timeout_ms"] = json!(3000);
    });
    let server = tokio::spawn(server.run_until(pending()));
    let summary = dir.path().join("summary.ndjson");
    let events = dir.path().join("events.ndjson");
    let plan = Plan::load(dir.path()).unwrap();
    let output = Output::open(&plan, &summary, Some(&events), 1.0, 0).unwrap();
    let started = Instant::now();
    let report = timeout(
        Duration::from_secs(10),
        run_with_output(plan, pending(), &output),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(5800));
    assert!(!output.finish(0, None).failed);
    server.abort();
    assert_eq!(report.offered, 4);
    assert_eq!(report.metrics.counters.started, 2);
    assert_eq!(report.metrics.counters.completed, 2);
    assert_eq!(report.warmup_metrics.counters.started, 2);
    assert_eq!(report.warmup_metrics.counters.completed, 2);
    assert_eq!(report.metrics.total.count(), 2);
    assert_eq!(report.warmup_metrics.total.count(), 2);
    assert_eq!(report.metrics.counters.connections_opened, 0);
    assert_eq!(report.warmup_metrics.counters.connections_opened, 1);
    assert_eq!(
        report.metrics.counters.connections_reused
            + report.warmup_metrics.counters.connections_reused,
        3
    );
    let summaries = records(&summary);
    let requests = records(&events);
    let phases = transitions(&summaries);
    assert_eq!(
        phases.iter().map(|p| p.0).collect::<Vec<_>>(),
        [
            Phase::Baseline,
            Phase::Warmup,
            Phase::Measure,
            Phase::Drain,
            Phase::Settle
        ]
    );
    assert_eq!(transitions(&requests), phases);
    assert!(phases[1].1 >= phases[0].1 + 980);
    assert!(phases[2].1 >= phases[1].1 + 980);
    assert!(phases[3].1 >= phases[2].1 + 980);
    assert!(phases[4].1 >= phases[3].1 + 1900);
    let mut measured = 0;
    let mut warmup = 0;
    let mut late_warmup = false;
    let mut last_annotation = None;
    for record in &summaries {
        match record {
            Record::Annotation(r) if r.code == "generator_self_metrics" => {
                last_annotation = r.detail.as_ref();
            }
            Record::Summary(r) => {
                let chain = &r.chains["ping"];
                if matches!(r.phase, Phase::Baseline | Phase::Settle) {
                    assert_eq!(chain.iterations_started, 0);
                    assert_eq!(chain.iterations_completed, 0);
                    assert_eq!(r.in_flight, 0);
                    assert_eq!(r.queue_depth, 0);
                    assert_eq!(r.target_rate, 0.0);
                }
                if r.phase == Phase::Warmup {
                    warmup += chain.duration.count;
                    if r.t_ms >= phases[2].1 && chain.duration.count > 0 {
                        late_warmup = true;
                        assert_eq!(r.target_rate, 0.0);
                        assert_eq!(last_annotation.unwrap()["timeline_phase"], json!("drain"));
                    }
                } else {
                    measured += chain.duration.count;
                }
            }
            _ => {}
        }
    }
    assert!(late_warmup);
    assert_eq!(warmup, 2);
    assert_eq!(measured, 2);
    let request_events: Vec<_> = requests
        .iter()
        .filter_map(|r| match r {
            Record::Request(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(
        request_events
            .iter()
            .filter(|r| r.phase == Phase::Warmup)
            .count(),
        2
    );
    assert_eq!(
        request_events
            .iter()
            .filter(|r| r.phase == Phase::Measure)
            .count(),
        2
    );
    assert!(
        request_events
            .iter()
            .all(|r| r.t_ms >= phases[3].1 && r.t_ms < phases[4].1)
    );
    assert!(
        matches!(summaries.last(), Some(Record::RunFinished(r)) if r.t_ms >= phases[4].1 + 980 && r.exit_code == 0)
    );
    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-contract");
    fs::create_dir_all(&archive).unwrap();
    fs::copy(summary, archive.join("phases-summary.ndjson")).unwrap();
    fs::copy(events, archive.join("phases-events.ndjson")).unwrap();
}

#[tokio::test]
async fn cancellation_flushes_each_reached_phase_without_emitting_future_transitions() {
    for (wanted, baseline, warmup, measure, settle, stop, latency) in [
        (Phase::Baseline, 1, 1, 1, 1, 100, 1500.0),
        (Phase::Warmup, 0, 1, 1, 1, 100, 1500.0),
        (Phase::Measure, 0, 0, 1, 1, 100, 1500.0),
        (Phase::Drain, 0, 0, 1, 1, 1100, 1500.0),
        (Phase::Settle, 0, 0, 1, 1, 1100, 10.0),
    ] {
        let server = MockServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            Config {
                latency: Latency::Fixed { ms: latency },
                ..Config::default()
            },
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
        support::edit(dir.path(), "mix.json", |doc| {
            doc["phases"] =
                json!({"baseline": format!("{baseline}s"), "settle": format!("{settle}s")});
            doc["load"]["warmup"] = json!(format!("{warmup}s"));
            doc["load"]["duration"] = json!(format!("{measure}s"));
            doc["load"]["rate"] = json!(2);
            doc["defaults"]["timeout_ms"] = json!(3000);
        });
        let server = tokio::spawn(server.run_until(pending()));
        let summary = dir.path().join("summary.ndjson");
        let events = dir.path().join("events.ndjson");
        let plan = Plan::load(dir.path()).unwrap();
        let sink = Output::open(&plan, &summary, Some(&events), 1.0, 0).unwrap();
        let report = run_with_output(plan, tokio::time::sleep(Duration::from_millis(stop)), &sink)
            .await
            .unwrap();
        assert!(report.interrupted, "{wanted:?}");
        assert_eq!(report.last_window.as_ref().unwrap().phase, wanted);
        assert_eq!(report.last_window.as_ref().unwrap().in_flight, 0);
        assert_eq!(report.last_window.as_ref().unwrap().queue_depth, 0);
        assert!(!sink.finish(130, Some("interrupted".into())).failed);
        server.abort();
        let records = records(&summary);
        assert_eq!(transitions(&records).last().unwrap().0, wanted);
        assert!(matches!(records.last(), Some(Record::RunFinished(r)) if r.exit_code == 130));
        assert_eq!(
            report.cancelled,
            report.metrics.counters.cancelled + report.warmup_metrics.counters.cancelled
        );
        if wanted == Phase::Warmup {
            assert!(report.warmup_metrics.counters.cancelled > 0);
            assert_eq!(report.metrics.counters.started, 0);
        }
        if wanted == Phase::Baseline {
            assert_eq!(report.admitted, 0);
        }
    }
}
