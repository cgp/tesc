mod support;

use metrix_engine::{Plan, Report, run};
use metrix_mock::{Config, ErrorInjection, Latency, MockServer};
use serde_json::json;
use std::{future::pending, time::Duration};
use support::{bundle, edit};
use tokio::{
    sync::oneshot,
    time::{Instant, timeout},
};

fn assert_accounting(report: &Report) {
    assert_eq!(
        report.offered,
        report.admitted
            + report.skipped_late
            + report.skipped_connections
            + report.skipped_concurrency
    );
    assert_eq!(
        report.admitted,
        report.responses + report.failed + report.cancelled
    );
    assert!(report.sent_finished <= report.responses + report.failed);
}

async fn against_mock(
    version: &str,
    config: Config,
    concurrency: usize,
    connections: usize,
    timeout_ms: u64,
) -> Report {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), config)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), server.local_addr().unwrap(), version);
    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["max_concurrency"] = json!(concurrency);
        doc["engine"]["connections_per_host"] = json!(connections);
        doc["defaults"]["timeout_ms"] = json!(timeout_ms);
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let report = timeout(
        Duration::from_secs(10),
        run(Plan::load(dir.path()).unwrap(), pending()),
    )
    .await
    .unwrap()
    .unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_accounting(&report);
    report
}

#[tokio::test]
async fn slow_responses_do_not_turn_fixed_rate_into_closed_model_load() {
    for version in ["http1", "http2"] {
        let report = against_mock(
            version,
            Config {
                latency: Latency::Fixed { ms: 100.0 },
                ..Config::default()
            },
            20,
            20,
            1000,
        )
        .await;
        assert_eq!(report.offered, 50);
        assert!(report.responses >= 45, "{report:?}"); // closed model would manage ~10.
        assert!(report.peak_in_flight >= 4 && report.peak_in_flight <= 20);
        assert_eq!(report.failed, 0);
    }
}

#[tokio::test]
async fn http2_multiplexes_with_only_one_server_connection() {
    let report = against_mock(
        "http2",
        Config {
            latency: Latency::Fixed { ms: 100.0 },
            max_connections: Some(1),
            ..Config::default()
        },
        20,
        1,
        1000,
    )
    .await;
    assert!(
        report.responses >= 45 && report.peak_in_flight >= 4,
        "{report:?}"
    );
    assert_eq!(report.skipped_connections, 0);
}

#[tokio::test]
async fn concurrency_and_http1_pool_caps_skip_instead_of_queueing() {
    for (concurrency, connections) in [(1, 20), (20, 1)] {
        let report = against_mock(
            "http1",
            Config {
                latency: Latency::Fixed { ms: 200.0 },
                ..Config::default()
            },
            concurrency,
            connections,
            1000,
        )
        .await;
        assert_eq!(report.offered, 50);
        assert!(report.responses >= 3 && report.responses <= 5, "{report:?}");
        assert_eq!(report.peak_in_flight, 1);
        assert!(report.skipped_connections + report.skipped_concurrency >= 40);
    }
}

#[tokio::test]
async fn timeouts_release_slots_and_keep_the_arrival_clock_running() {
    for version in ["http1", "http2"] {
        let report = against_mock(
            version,
            Config {
                latency: Latency::Fixed { ms: 250.0 },
                ..Config::default()
            },
            20,
            20,
            40,
        )
        .await;
        assert_eq!(report.offered, 50);
        assert_eq!(report.responses, 0);
        assert_eq!(report.failed, report.timed_out);
        assert_eq!(report.admitted, report.timed_out);
        assert!(report.admitted >= 45, "{report:?}");
    }
}

#[tokio::test]
async fn http_status_errors_are_responses_and_transport_errors_recover_without_retries() {
    for version in ["http1", "http2"] {
        let report = against_mock(
            version,
            Config {
                latency: Latency::Fixed { ms: 0.0 },
                errors: vec![
                    ErrorInjection::Http {
                        rate: 0.5,
                        status: 503,
                    },
                    ErrorInjection::Disconnect { rate: 0.5 },
                ],
                ..Config::default()
            },
            20,
            20,
            1000,
        )
        .await;
        assert!(report.responses > 5 && report.failed > 5, "{report:?}");
        assert_eq!(report.admitted, report.sent_finished); // one send per admitted arrival, never a retry.
    }
}

#[tokio::test]
async fn cancellation_interrupts_long_requests_and_closes_connections() {
    for version in ["http1", "http2"] {
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
        bundle(dir.path(), server.local_addr().unwrap(), version);
        edit(dir.path(), "mix.json", |doc| {
            doc["defaults"]["timeout_ms"] = json!(10000)
        });
        let task = tokio::spawn(server.run_until(pending()));
        let start = Instant::now();
        let report = timeout(
            Duration::from_secs(2),
            run(Plan::load(dir.path()).unwrap(), async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            report.interrupted && report.cancelled > 0 && start.elapsed() < Duration::from_secs(2),
            "{report:?}"
        );
        assert_accounting(&report);
        task.abort();
    }
}

#[tokio::test]
async fn unavailable_target_fails_setup_without_starting_the_load_clock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    let error = run(Plan::load(dir.path()).unwrap(), pending())
        .await
        .unwrap_err();
    // Some platforms defer a refused connect beyond the configured setup deadline.
    assert!(
        error.contains("Connect") || error.contains("Timeout"),
        "{error}"
    );
}
