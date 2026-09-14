//! B3.4: what a step has to look like to have succeeded, and what happens when it
//! does not.

mod support;

use std::{
    convert::Infallible,
    future::pending,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, Report, run};
use serde_json::{Value, json};
use support::{bundle, edit};
use tokio::net::TcpListener;

/// A service that answers by path: a 401, a job that finishes on its third ask, and
/// an ordinary JSON document.
///
/// The job's state is per connection rather than global. An iteration holds one
/// connection for its whole chain (design-engine §5), so counting there makes every
/// iteration take exactly three asks however many run at once — the alternative is a
/// test whose expected numbers depend on the scheduler's interleaving.
async fn serve(polls: Arc<AtomicU64>) -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let polls = Arc::clone(&polls);
            let asked = Arc::new(AtomicU64::new(0));
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let polls = Arc::clone(&polls);
                    let asked = Arc::clone(&asked);
                    async move {
                        let path = request.uri().path().to_owned();
                        let response = match path.as_str() {
                            // A deliberately-failing flow: the 401 is the point.
                            "/login-bad" => Response::builder()
                                .status(401)
                                .body(Full::new(Bytes::from_static(b"no")))
                                .unwrap(),
                            // Two "running" answers, then "complete".
                            "/job" => {
                                polls.fetch_add(1, Ordering::Relaxed);
                                let seen = asked.fetch_add(1, Ordering::Relaxed);
                                let state = if seen % 3 == 2 { "complete" } else { "running" };
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(format!(
                                        "{{\"status\":\"{state}\"}}"
                                    ))))
                                    .unwrap()
                            }
                            "/items" => Response::builder()
                                .status(200)
                                .header("content-type", "application/json; charset=utf-8")
                                .body(Full::new(Bytes::from_static(
                                    br#"{"items":[1,2,3],"name":"widget"}"#,
                                )))
                                .unwrap(),
                            _ => Response::builder()
                                .status(500)
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
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

fn one_call(root: &Path, address: SocketAddr, call: Value, step: Value) {
    bundle(root, address, "http1");
    support::write(root, "calls/ping.json", &json!({"only": call}));
    edit(root, "mix.json", |doc| {
        let mut only = json!({"id": "only", "call": "only"});
        for (key, value) in step.as_object().expect("a step object") {
            only[key] = value.clone();
        }
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh", "steps": [only]
        }]);
        doc["load"]["rate"] = json!(20);
        doc["load"]["duration"] = json!("1s");
    });
}

async fn run_call(call: Value, step: Value) -> (Report, Arc<AtomicU64>) {
    let polls = Arc::new(AtomicU64::new(0));
    let address = serve(Arc::clone(&polls)).await;
    let dir = tempfile::tempdir().unwrap();
    one_call(dir.path(), address, call, step);
    let plan = Plan::load(dir.path()).unwrap();
    (run(plan, pending::<()>()).await.unwrap(), polls)
}

#[tokio::test]
async fn a_chain_that_expects_a_401_passes_when_it_gets_one() {
    let (report, _) = run_call(
        json!({"method": "POST", "path": "/login-bad", "assert": [{"status": 401}]}),
        json!({}),
    )
    .await;
    let step = &report.metrics.chains["chain"].steps["only"];
    // A generator that called its own expectation an error would report a working
    // service as broken.
    assert!(step.completed > 0);
    assert_eq!(step.failed, 0);
    assert_eq!(report.metrics.chains["chain"].aborted, 0);
}

#[tokio::test]
async fn an_answer_that_is_not_the_one_asked_for_fails_by_index() {
    let (report, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [
            {"status": 200},
            {"json": "$.name", "equals": "sprocket"}
        ]}),
        json!({}),
    )
    .await;
    let step = &report.metrics.chains["chain"].steps["only"];
    assert!(step.failed > 0);
    assert_eq!(step.completed, 0);
    // The second assertion, named by its index in the call rather than by a message.
    assert_eq!(step.assertion_failures.keys().collect::<Vec<_>>(), [&1]);
    // The status still counted: the request happened and the service answered.
    assert_eq!(step.statuses[&200], step.attempted);
}

#[tokio::test]
async fn a_failed_assertion_is_not_a_transport_failure() {
    let (report, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [{"status": 204}]}),
        json!({}),
    )
    .await;
    let step = &report.metrics.chains["chain"].steps["only"];
    assert!(step.failed > 0);
    // Counted under its own class, not as a connection or a timeout.
    assert_eq!(step.errors[8], step.failed, "assertion failures");
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn a_length_is_the_shape_s_own_rather_than_its_serialisation_s() {
    // `$.items` holds three entries; the text it serialises to is seven characters.
    let (passes, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [
            {"json": "$.items", "min_length": 3, "max_length": 3}
        ]}),
        json!({}),
    )
    .await;
    assert_eq!(passes.metrics.chains["chain"].steps["only"].failed, 0);

    let (fails, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [
            {"json": "$.items", "min_length": 4}
        ]}),
        json!({}),
    )
    .await;
    assert!(fails.metrics.chains["chain"].steps["only"].failed > 0);
}

#[tokio::test]
async fn a_content_type_holds_whatever_parameters_the_server_attached() {
    // `application/json; charset=utf-8` satisfies `application/json`: the parameters
    // are the server's business and the type is what was asked about.
    let (report, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [
            {"content_type": "application/json"}
        ]}),
        json!({}),
    )
    .await;
    assert_eq!(report.metrics.chains["chain"].steps["only"].failed, 0);
}

#[tokio::test]
async fn continue_carries_on_and_abort_does_not() {
    for (policy, second_ran) in [("continue", true), ("abort", false)] {
        let polls = Arc::new(AtomicU64::new(0));
        let address = serve(Arc::clone(&polls)).await;
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), address, "http1");
        support::write(
            dir.path(),
            "calls/ping.json",
            &json!({
                "bad": {"method": "GET", "path": "/items", "assert": [{"status": 204}]},
                "good": {"method": "GET", "path": "/items"},
            }),
        );
        edit(dir.path(), "mix.json", |doc| {
            doc["chains"] = json!([{
                "name": "chain", "percent": 100, "session": "fresh",
                "steps": [
                    {"id": "first", "call": "bad", "on_failure": policy},
                    {"id": "second", "call": "good"}
                ]
            }]);
            doc["load"]["rate"] = json!(20);
            doc["load"]["duration"] = json!("1s");
        });
        let report = run(Plan::load(dir.path()).unwrap(), pending::<()>())
            .await
            .unwrap();
        let chain = &report.metrics.chains["chain"];
        assert_eq!(
            chain.steps["second"].attempted > 0,
            second_ran,
            "on_failure: {policy}"
        );
    }
}

#[tokio::test]
async fn a_retry_gives_the_step_one_more_chance_and_counts_both() {
    let (report, _) = run_call(
        json!({"method": "GET", "path": "/items", "assert": [{"status": 204}]}),
        json!({"on_failure": "retry"}),
    )
    .await;
    let chain = &report.metrics.chains["chain"];
    // Two attempts per iteration, both counted: they were both real requests.
    assert_eq!(chain.steps["only"].attempted, chain.started * 2);
    assert_eq!(chain.steps["only"].failed, chain.steps["only"].attempted);
}

#[tokio::test]
async fn polling_keeps_asking_until_the_answer_says_it_is_done() {
    let (report, polls) = run_call(
        json!({"method": "GET", "path": "/job"}),
        json!({"repeat_until": {
            "json": "$.status", "equals": "complete",
            "max_attempts": 5, "interval_ms": 1
        }}),
    )
    .await;
    let chain = &report.metrics.chains["chain"];
    // Three asks each, and it stopped at the third rather than running to the
    // ceiling: the answer ended the loop, not the count.
    assert_eq!(polls.load(Ordering::Relaxed), chain.started * 3);
    // Every attempt is a real request and is counted as one; what is kept out of
    // request latency is the waiting between them.
    assert_eq!(chain.steps["only"].attempted, chain.started * 3);
    assert_eq!(chain.steps["only"].failed, 0);
    assert_eq!(chain.aborted, 0);
}

#[tokio::test]
async fn a_job_that_never_finishes_is_not_a_step_that_succeeded() {
    let (report, _) = run_call(
        json!({"method": "GET", "path": "/job"}),
        json!({"repeat_until": {
            // Nothing will ever equal this, so only `max_attempts` ends it.
            "json": "$.status", "equals": "never",
            "max_attempts": 3, "interval_ms": 1
        }}),
    )
    .await;
    let chain = &report.metrics.chains["chain"];
    let step = &chain.steps["only"];
    // Exactly the ceiling, per iteration. Polling that never stops is a run that
    // never ends.
    assert_eq!(step.attempted, chain.started * 3);
    // And giving up is not finishing. A step that polled three times and never saw
    // the job complete has not seen it complete, and counting that as a success
    // would report a service that finishes nothing as healthy.
    assert_eq!(step.failed, chain.started);
    assert_eq!(
        step.errors[8], step.failed,
        "counted as an unmet expectation"
    );
    // Under no assertion index, because no assertion was written: the expectation
    // that went unmet is `repeat_until`'s own.
    assert!(step.assertion_failures.is_empty());
    assert_eq!(chain.aborted, chain.started);
}

#[test]
fn a_selector_with_nothing_asked_of_it_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    one_call(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({"method": "GET", "path": "/", "assert": [{"json": "$.id"}]}),
        json!({}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // Vacuously true is a plan error wearing the clothes of a passing test.
    assert!(error.contains("no condition"), "{error}");
}
