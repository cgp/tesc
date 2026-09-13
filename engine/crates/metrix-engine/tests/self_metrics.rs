mod support;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{server::conn::http2, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, run_with_snapshots};
use metrix_metrics::aggregation::Window;
use metrix_mock::{Config, Latency, MockServer};
use serde_json::json;
use std::{convert::Infallible, future::pending, time::Duration};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};

fn collect(mut receiver: mpsc::Receiver<Window>) -> Vec<Window> {
    let mut windows = Vec::new();
    while let Ok(window) = receiver.try_recv() {
        windows.push(window);
    }
    windows
}

#[tokio::test]
async fn send_drift_is_visible_before_responses_and_survives_final_drain() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: Latency::Fixed { ms: 1500.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["defaults"]["timeout_ms"] = json!(3000);
    });
    let server = tokio::spawn(server.run_until(pending()));
    let (sender, receiver) = mpsc::channel(32);
    let report = timeout(
        Duration::from_secs(5),
        run_with_snapshots(Plan::load(dir.path()).unwrap(), pending(), Some(sender)),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    let windows = collect(receiver);
    let first = &windows[0];
    assert!(first.in_flight >= 10);
    assert_eq!(first.queue_depth, 0);
    assert_eq!(first.metrics.total.count(), 0);
    assert!(first.metrics.drift.count() >= 10);
    assert!(windows.iter().all(|w| w.queue_depth <= w.in_flight));
    assert!(
        windows
            .iter()
            .any(|w| w.metrics.counters.completed > 0 && w.metrics.drift.count() == 0)
    );
    assert_eq!(
        windows.iter().map(|w| w.metrics.drift.count()).sum::<u64>(),
        report.sent
    );
    assert_eq!(report.sent, report.sent_finished);
    assert_eq!(report.metrics.drift.count(), report.metrics.total.count());
    assert_eq!(windows.last().unwrap().in_flight, 0);
    assert_eq!(windows.last().unwrap().queue_depth, 0);
}

#[tokio::test]
async fn peer_stream_limits_are_not_reported_as_pre_hyper_send_queue() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        http2::Builder::new(TokioExecutor::new())
            .max_concurrent_streams(0)
            .serve_connection(
                TokioIo::new(socket),
                service_fn(|_| async {
                    Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::new())))
                }),
            )
            .await
    });
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), address, "http2");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["max_concurrency"] = json!(4);
        doc["defaults"]["timeout_ms"] = json!(600);
    });
    let (sender, receiver) = mpsc::channel(32);
    let report = timeout(
        Duration::from_secs(4),
        run_with_snapshots(Plan::load(dir.path()).unwrap(), pending(), Some(sender)),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    let windows = collect(receiver);
    assert!(windows.iter().all(|w| w.queue_depth == 0));
    assert!(windows.iter().all(|w| w.queue_depth <= w.in_flight));
    assert_eq!(report.sent, report.admitted);
    assert_eq!(report.metrics.drift.count(), report.sent);
    assert_eq!(report.metrics.total.count(), report.sent_finished);
    assert_eq!(report.metrics.ttfb.count(), 0);
    assert!(report.timed_out > 0 && report.skipped_concurrency > 0);
    assert_eq!(windows.last().unwrap().queue_depth, 0);
    assert_eq!(windows.last().unwrap().in_flight, 0);
}

#[tokio::test]
async fn scheduler_lag_measures_executor_stalls_and_coalesces_missed_ticks() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http2");
    let server = tokio::spawn(server.run_until(pending()));
    let (sender, receiver) = mpsc::channel(32);
    let report = run_with_snapshots(
        Plan::load(dir.path()).unwrap(),
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Deliberately stall this current-thread runtime across two snapshot deadlines.
            std::thread::sleep(Duration::from_millis(400));
            pending().await
        },
        Some(sender),
    )
    .await
    .unwrap();
    server.abort();
    let windows = collect(receiver);
    assert!(report.max_scheduler_lag >= Duration::from_millis(300));
    assert!(report.skipped_late > 0);
    assert_eq!(
        windows.iter().map(|w| w.scheduler_lag_samples).sum::<u64>(),
        report.scheduler_lag_samples
    );
    assert_eq!(
        windows.iter().map(|w| w.scheduler_lag).max().unwrap(),
        report.max_scheduler_lag
    );
    assert!(windows.iter().all(|w| w.scheduler_lag_samples <= 1));
    assert!(report.scheduler_lag_samples < 4);
}
