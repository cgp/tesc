mod support;

use metrix_engine::{Plan, Report, run_with_snapshots};
use metrix_metrics::aggregation::{Accumulator, Window};
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::{future::pending, time::Duration};
use tokio::{sync::mpsc, time::timeout};

async fn run_mock(sender: mpsc::Sender<Window>, delay_ms: f64, stop_ms: Option<u64>) -> Report {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: delay_ms },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["duration"] = json!("2s");
        doc["defaults"]["timeout_ms"] = json!(2000);
    });
    let task = tokio::spawn(server.run_until(pending()));
    let shutdown = async move {
        match stop_ms {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => pending().await,
        }
    };
    let report = timeout(
        Duration::from_secs(6),
        run_with_snapshots(Plan::load(dir.path()).unwrap(), shutdown, Some(sender)),
    )
    .await
    .unwrap()
    .unwrap();
    task.abort();
    report
}

#[tokio::test]
async fn interval_windows_conserve_samples_and_include_final_drain() {
    let (sender, mut receiver) = mpsc::channel(32);
    let report = run_mock(sender, 350.0, None).await;
    let mut merged = Accumulator::default();
    let mut windows = 0;
    let mut previous = Duration::ZERO;
    while let Some(window) = receiver.recv().await {
        assert_eq!(window.from, previous);
        assert!(window.to > window.from);
        merged.merge(&window.metrics);
        previous = window.to;
        windows += 1;
    }
    assert!(windows >= 8, "{windows}");
    assert_eq!(report.windows, windows);
    assert_eq!(report.windows_dropped, 0);
    assert_eq!(merged.counters.started, report.admitted);
    assert_eq!(merged.counters.completed, report.responses);
    assert_eq!(merged.counters.connections_opened, 1); // HTTP/2, one setup socket.
    assert_eq!(merged.counters.connections_reused, report.sent_finished - 1);
    assert_eq!(merged.counters.statuses[200], report.responses);
    assert_eq!(merged.total.snapshot(), report.metrics.total.snapshot());
    assert_eq!(merged.chain.snapshot(), report.metrics.chain.snapshot());
    assert_eq!(merged.ttfb.count(), report.responses);
    assert_eq!(merged.counters.bytes_received, report.responses * 12); // JSON response payload.
    let last = report.last_window.unwrap();
    assert_eq!(last.in_flight, 0);
    assert!(last.to > Duration::from_secs(2));
    assert!(last.metrics.counters.completed > 0);
    assert!(report.metrics.total.snapshot().min_us.unwrap() >= 350_000);
}

#[tokio::test]
async fn stalled_and_closed_snapshot_consumers_drop_windows_without_stalling_traffic() {
    for closed in [false, true] {
        let (sender, receiver) = mpsc::channel(1);
        let retained = if closed {
            drop(receiver);
            None
        } else {
            Some(receiver)
        };
        let report = run_mock(sender, 10.0, None).await;
        assert_eq!(report.offered, 100);
        assert!(report.responses >= 98, "responses={}", report.responses);
        assert_eq!(report.failed, 0);
        assert!(report.windows >= 8);
        assert_eq!(report.windows_dropped, report.windows - u64::from(!closed));
        assert_eq!(report.metrics.total.count(), report.responses);
        drop(retained);
    }
}

#[tokio::test]
async fn cancellation_flushes_partial_window_without_fabricating_latencies() {
    let (sender, mut receiver) = mpsc::channel(8);
    let report = run_mock(sender, 1500.0, Some(100)).await;
    assert!(report.interrupted && report.cancelled > 0);
    assert_eq!(report.windows, 1);
    let window = receiver.recv().await.unwrap();
    assert_eq!(window.from, Duration::ZERO);
    assert!(window.to < Duration::from_millis(250));
    assert_eq!(window.in_flight, 0);
    assert_eq!(window.queue_depth, 0);
    assert_eq!(window.metrics.counters.started, report.admitted);
    assert_eq!(window.metrics.counters.cancelled, report.cancelled);
    assert_eq!(report.metrics.total.count(), 0);
    assert_eq!(report.metrics.chain.count(), 0);
    assert!(report.sent > 0);
    assert_eq!(report.metrics.drift.count(), report.sent);
    assert!(receiver.recv().await.is_none());
}
