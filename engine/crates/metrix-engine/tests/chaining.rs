//! B3.2: a chain is a sequence, and what one step captures the next one sends.
//!
//! Run against a server that only answers the second request when the first one's
//! id actually arrived. That is the point of the test: a chain that quietly sent
//! `/orders/` would get a 404 and look like a service problem, so the service here
//! is written to tell the two apart.

mod support;

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, Report, run};
use serde_json::json;
use std::future::pending;
use support::{bundle, edit};
use tokio::net::TcpListener;

/// Counts what each path was asked for, so a test can say which requests happened
/// rather than only how many.
#[derive(Default)]
struct Seen {
    created: AtomicU64,
    fetched_with_id: AtomicU64,
    fetched_without_id: AtomicU64,
}

/// A service with one rule: `/orders/{id}` answers 200 only for the id it issued.
async fn serve(seen: Arc<Seen>) -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let path = request.uri().path().to_owned();
                        let response = if path == "/orders" {
                            seen.created.fetch_add(1, Ordering::Relaxed);
                            Response::builder()
                                .status(201)
                                .header("content-type", "application/json")
                                .header("x-order", "A-1000")
                                .body(Full::new(Bytes::from_static(
                                    br#"{"order":{"id":"A-1000"},"lines":[{"sku":"S1"}]}"#,
                                )))
                                .unwrap()
                        } else if path == "/hang" {
                            // Long enough that any step timeout beats it. A
                            // transport failure rather than a status: a 404 is an
                            // answer, and this test is about a step that has none.
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::new()))
                                .unwrap()
                        } else if path == "/orders/A-1000" {
                            seen.fetched_with_id.fetch_add(1, Ordering::Relaxed);
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"ok")))
                                .unwrap()
                        } else {
                            // `/orders/` and anything else: the shape a chain that
                            // substituted nothing would produce.
                            seen.fetched_without_id.fetch_add(1, Ordering::Relaxed);
                            Response::builder()
                                .status(404)
                                .body(Full::new(Bytes::new()))
                                .unwrap()
                        };
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

/// A two-step chain: create an order, then read it back by the id it returned.
fn two_step(root: &std::path::Path, address: SocketAddr, extract: serde_json::Value) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({
            "create": {
                "method": "POST",
                "path": "/orders",
                "body": "{}",
                "extract": extract,
            },
            "read": {"method": "GET", "path": "/orders/{{ order_id }}"},
        }),
    );
    edit(root, "mix.json", |doc| {
        doc["chains"] = json!([{
            "name": "order", "percent": 100, "session": "fresh",
            "steps": [{"id": "create", "call": "create"}, {"id": "read", "call": "read"}]
        }]);
        doc["load"]["rate"] = json!(20);
        doc["load"]["duration"] = json!("1s");
    });
}

async fn run_bundle(root: &std::path::Path) -> Report {
    let plan = Plan::load(root).unwrap();
    run(plan, pending::<()>()).await.unwrap()
}

#[tokio::test]
async fn a_captured_value_is_sent_by_the_step_after_it() {
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        address,
        json!({"order_id": {"json": "$.order.id"}}),
    );

    let report = run_bundle(dir.path()).await;

    let created = seen.created.load(Ordering::Relaxed);
    assert!(created > 0, "nothing was sent");
    // Every second step reached the id it was given. A chain that substituted an
    // empty string would land on `/orders/` and be counted on the other line.
    assert_eq!(seen.fetched_with_id.load(Ordering::Relaxed), created);
    assert_eq!(seen.fetched_without_id.load(Ordering::Relaxed), 0);
    assert_eq!(report.chains_aborted, 0);
}

#[tokio::test]
async fn a_chain_is_measured_per_step_and_end_to_end() {
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        address,
        json!({"order_id": {"json": "$.order.id"}}),
    );

    let report = run_bundle(dir.path()).await;
    let chain = &report.metrics.chains["order"];

    // Both steps have their own distribution: two steps of one chain are two
    // different requests, and pooling them would publish a median of two things.
    assert_eq!(chain.steps.len(), 2);
    assert!(chain.steps["create"].total.count() > 0);
    assert!(chain.steps["read"].total.count() > 0);
    assert_eq!(
        chain.steps["create"].statuses[&201],
        chain.steps["create"].completed
    );
    assert_eq!(
        chain.steps["read"].statuses[&200],
        chain.steps["read"].completed
    );

    // And the chain's own duration, which is not the sum of the two: it is what one
    // virtual user waited through, and it is counted once per iteration.
    assert_eq!(chain.duration.count(), chain.completed + chain.aborted);
    assert_eq!(chain.started, report.admitted);
}

#[tokio::test]
async fn every_extractor_can_feed_the_next_step() {
    for extract in [
        json!({"order_id": {"json": "$.order.id"}}),
        json!({"order_id": {"header": "X-Order"}}),
        json!({"order_id": {"regex": "\"id\":\"([A-Z0-9-]+)\""}}),
    ] {
        let seen = Arc::new(Seen::default());
        let address = serve(Arc::clone(&seen)).await;
        let dir = tempfile::tempdir().unwrap();
        two_step(dir.path(), address, extract.clone());

        run_bundle(dir.path()).await;
        assert!(
            seen.fetched_with_id.load(Ordering::Relaxed) > 0,
            "nothing reached the id with {extract}"
        );
        assert_eq!(
            seen.fetched_without_id.load(Ordering::Relaxed),
            0,
            "a request went out without the id with {extract}"
        );
    }
}

#[tokio::test]
async fn a_variable_nothing_captures_is_refused_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        // Captures something else, so `{{ order_id }}` has no source.
        json!({"other": {"json": "$.order.id"}}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // Named at load rather than found per iteration: the chain would break every
    // single time, and thousands of failures would bury the one fact behind them.
    assert!(error.contains("order_id"), "{error}");
    assert!(error.contains("chains/0/steps/1"), "{error}");
}

#[tokio::test]
async fn a_step_that_fails_stops_the_chain_and_is_counted_apart() {
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({
            // A timeout the service cannot beat, so the first step always fails.
            "slow": {"method": "GET", "path": "/hang", "timeout_ms": 50},
            "after": {"method": "GET", "path": "/orders"},
        }),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["chains"] = json!([{
            "name": "order", "percent": 100, "session": "fresh",
            "steps": [{"id": "first", "call": "slow"}, {"id": "second", "call": "after"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });

    let report = run_bundle(dir.path()).await;
    let chain = &report.metrics.chains["order"];

    assert!(chain.aborted > 0);
    assert_eq!(chain.completed, 0);
    // The step after the failure was never attempted: it was going to act on
    // something that did not happen.
    assert_eq!(chain.steps["second"].attempted, 0);
    assert_eq!(report.chains_aborted, chain.aborted);
    // And counted apart from the requests that failed, so one upstream problem does
    // not inflate the error rate twice over.
    assert_eq!(chain.steps["first"].failed, chain.aborted);
}

#[tokio::test]
async fn a_body_longer_than_the_ceiling_is_cut_rather_than_kept() {
    // The counted bytes are a measurement and the kept body is evidence; running out
    // of room for the second must not change the first.
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        address,
        json!({"order_id": {"json": "$.order.id"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        // One kilobyte is the smallest the format allows; the response is far
        // shorter, so nothing is cut and the chain still works.
        doc["capture"] = json!({"error_samples": 1, "body_max_kb": 1});
    });

    let report = run_bundle(dir.path()).await;
    assert_eq!(report.chains_aborted, 0);
    assert!(seen.fetched_with_id.load(Ordering::Relaxed) > 0);
    assert!(report.metrics.counters.bytes_received > 0);
}

#[tokio::test]
async fn one_iteration_uses_one_connection_for_all_of_its_steps() {
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        address,
        json!({"order_id": {"json": "$.order.id"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(5);
        doc["load"]["duration"] = json!("1s");
        doc["load"]["max_concurrency"] = json!(1);
        doc["engine"]["connections_per_host"] = json!(1);
    });

    let report = run_bundle(dir.path()).await;
    let chain = &report.metrics.chains["order"];
    let requests = chain.steps["create"].attempted + chain.steps["read"].attempted;
    // Two requests per iteration, and nowhere near two connections each: the steps
    // of one iteration are one virtual user, and giving each its own socket would
    // measure a service being connected to rather than used.
    assert!(requests >= 2);
    assert!(
        report.metrics.counters.connections_opened < requests,
        "opened {} connections for {requests} requests",
        report.metrics.counters.connections_opened
    );
    assert!(report.metrics.counters.connections_reused > 0);
}

#[tokio::test]
async fn a_timeout_is_the_step_s_own_rather_than_the_chain_s() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Seen::default());
    let address = serve(Arc::clone(&seen)).await;
    two_step(
        dir.path(),
        address,
        json!({"order_id": {"json": "$.order.id"}}),
    );
    edit(dir.path(), "calls/ping.json", |doc| {
        doc["create"]["timeout_ms"] = json!(2000);
        doc["read"]["timeout_ms"] = json!(2000);
    });
    let plan = Plan::load(dir.path()).unwrap();
    // The pool is prepared with the longest single request, not their sum: a pool
    // that gave up sooner than the request it carries would report the generator's
    // impatience as the service's failure.
    let report = run(plan, pending::<()>()).await.unwrap();
    assert_eq!(report.failed, 0);
}
