//! B3.5: a plan that varies its traffic from a file and from the fixed function set.
//!
//! Run against a service that records the exact path and body of every request, so a
//! test can say *which* rows were sent and in what order rather than only that
//! something was sent.

mod support;

use std::{
    convert::Infallible,
    fs,
    future::pending,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
};

use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, run};
use serde_json::{Value, json};
use support::{bundle, edit};
use tokio::net::TcpListener;

/// Every request, in the order the service answered it.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

async fn serve(seen: Seen) -> SocketAddr {
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
                        let path = request.uri().to_string();
                        let body = request
                            .into_body()
                            .collect()
                            .await
                            .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
                            .unwrap_or_default();
                        seen.lock().expect("the recorder").push((path, body));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"ok")))
                                .unwrap(),
                        )
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

/// A bundle with one dataset file and one call that uses it.
fn with_dataset(
    root: &Path,
    address: SocketAddr,
    file: &str,
    contents: &str,
    spec: Value,
    call: Value,
) {
    bundle(root, address, "http1");
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join(file), contents).unwrap();
    support::write(root, "calls/ping.json", &json!({"only": call}));
    edit(root, "mix.json", |doc| {
        doc["datasets"] = spec;
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh",
            "steps": [{"id": "only", "call": "only"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
}

const USERS: &str = "email,region\na@x.test,apac\nb@x.test,emea\nc@x.test,amer\n";

#[tokio::test]
async fn a_row_reaches_the_path_the_query_and_the_body() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        address,
        "data/users.csv",
        USERS,
        json!({"users": {"file": "data/users.csv", "mode": "round_robin"}}),
        json!({
            "method": "POST",
            "path": "/u/{{ users.email }}",
            "query": {"region": "{{ users.region }}"},
            "body": "{\"to\":\"{{ users.email }}\"}",
        }),
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    for (path, body) in &seen {
        // One row per iteration, everywhere in the request. A path and a body that
        // disagreed would be sending two users in one request.
        let email = path
            .strip_prefix("/u/")
            .and_then(|rest| rest.split('?').next())
            .expect("a path built from the row");
        assert!(body.contains(email), "{path} sent {body}");
        let region = match email {
            "a@x.test" => "apac",
            "b@x.test" => "emea",
            _ => "amer",
        };
        assert!(path.ends_with(&format!("?region={region}")), "{path}");
    }
}

#[tokio::test]
async fn round_robin_walks_the_file_in_order() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        address,
        "data/users.csv",
        USERS,
        json!({"users": {"file": "data/users.csv", "mode": "round_robin"}}),
        json!({"method": "GET", "path": "/u/{{ users.email }}"}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let sent: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    // In order and wrapping, across the whole run rather than per worker: a cursor
    // owned by each worker would visit every third row on a three-worker machine.
    let expected: Vec<String> = (0..sent.len())
        .map(|i| format!("/u/{}", ["a@x.test", "b@x.test", "c@x.test"][i % 3]))
        .collect();
    assert_eq!(sent, expected);
}

#[tokio::test]
async fn unique_per_iteration_refuses_a_file_that_would_run_out() {
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "data/users.csv",
        USERS,
        json!({"users": {"file": "data/users.csv", "mode": "unique_per_iteration"}}),
        json!({"method": "POST", "path": "/u/{{ users.email }}"}),
    );
    // Ten iterations a second for a second, against three rows. Wrapping would take
    // away the one thing the mode promises, so the plan is refused with both numbers.
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(
        error.contains("starts 10") && error.contains("3 rows"),
        "{error}"
    );
}

#[tokio::test]
async fn a_dataset_nothing_reads_is_not_counted_against_the_run() {
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "data/users.csv",
        USERS,
        json!({"users": {"file": "data/users.csv", "mode": "unique_per_iteration"}}),
        json!({"method": "GET", "path": "/ping"}),
    );
    // Declared and unused: three rows are enough for the zero iterations that read
    // them, and refusing here would be refusing over a row that is never sent.
    Plan::load(dir.path()).map(drop).unwrap();
}

#[tokio::test]
async fn jsonl_rows_read_the_same_way_csv_rows_do() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        address,
        "data/users.jsonl",
        "{\"id\": 1, \"tier\": \"gold\"}\n{\"id\": 2, \"tier\": \"silver\"}\n",
        json!({"users": {"file": "data/users.jsonl", "mode": "round_robin"}}),
        json!({"method": "GET", "path": "/u/{{ users.id }}/{{ users.tier }}"}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let sent: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(sent.contains(&"/u/1/gold".to_owned()));
    assert!(sent.contains(&"/u/2/silver".to_owned()));
}

#[tokio::test]
async fn the_inline_functions_reach_the_request() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"only": {
            "method": "POST",
            "path": "/o/{{ uuid() }}",
            "query": {"n": "{{ rand(1,3) }}", "tier": "{{ pick('gold','silver') }}"},
            "body": "{\"seq\":{{ seq() }},\"at\":\"{{ now('unix') }}\"}",
        }}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh",
            "steps": [{"id": "only", "call": "only"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    let mut ids = std::collections::BTreeSet::new();
    let mut sequences = Vec::new();
    for (path, body) in &seen {
        let (id, query) = path
            .strip_prefix("/o/")
            .and_then(|rest| rest.split_once('?'))
            .expect("a generated path");
        assert_eq!(id.len(), 36, "{path}");
        ids.insert(id.to_owned());
        let number: u32 = query
            .split_once("n=")
            .and_then(|(_, rest)| rest.split('&').next())
            .and_then(|n| n.parse().ok())
            .expect("a number");
        assert!((1..=3).contains(&number), "{path}");
        assert!(
            query.contains("tier=gold") || query.contains("tier=silver"),
            "{path}"
        );
        let sent: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
        sequences.push(sent["seq"].as_u64().expect("a sequence number"));
        assert!(sent["at"].as_str().expect("a timestamp").len() >= 10);
    }
    // A uuid per iteration, all different: a plan posting orders wants ids that do
    // not collide, and a generator reusing one would be sending the same order twice.
    assert_eq!(ids.len(), seen.len());
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=seen.len() as u64).collect::<Vec<_>>());
}

#[tokio::test]
async fn the_same_seed_sends_the_same_requests_twice() {
    let send = |seed| async move {
        let seen: Seen = Arc::default();
        let address = serve(Arc::clone(&seen)).await;
        let dir = tempfile::tempdir().unwrap();
        with_dataset(
            dir.path(),
            address,
            "data/users.csv",
            USERS,
            json!({"users": {"file": "data/users.csv", "mode": "random"}}),
            json!({
                "method": "POST",
                "path": "/u/{{ users.email }}",
                "body": "{{ pick('a','b','c','d') }}{{ rand(1,1000000) }}",
            }),
        );
        let mut plan = Plan::load(dir.path()).unwrap();
        plan.set_seed(seed);
        run(plan, pending::<()>()).await.unwrap();
        let mut sent = seen.lock().unwrap().clone();
        // By path, because the order two runs complete in is the service's business.
        sent.sort();
        sent
    };

    // The whole reason the seed is recorded in the run's identity: two runs of one
    // plan are comparable only if they sent the same traffic.
    assert_eq!(send(11).await, send(11).await);
    assert_ne!(send(11).await, send(12).await);
}

#[test]
fn a_reference_to_a_field_the_file_does_not_have_is_refused_at_load() {
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "data/users.csv",
        USERS,
        json!({"users": {"file": "data/users.csv", "mode": "round_robin"}}),
        json!({"method": "GET", "path": "/u/{{ users.emial }}"}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // Naming what the file does have, because this error is almost always a typo.
    assert!(
        error.contains("emial") && error.contains("email, region"),
        "{error}"
    );
}

#[test]
fn a_dataset_outside_the_bundle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_dataset(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "data/users.csv",
        USERS,
        json!({"users": {"file": "../users.csv", "mode": "round_robin"}}),
        json!({"method": "GET", "path": "/u/{{ users.email }}"}),
    );
    fs::write(dir.path().parent().unwrap().join("users.csv"), USERS).unwrap();
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // The bundle is the unit that gets copied to a load box; a plan reaching outside
    // it runs here and not there.
    assert!(error.contains("outside the bundle"), "{error}");
}

#[tokio::test]
async fn a_call_that_only_generates_is_still_not_a_fixed_request() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"ping": {"method": "GET", "path": "/p/{{ seq() }}"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let sent: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert!(sent.len() > 1);
    // Compiled once and sent unchanged would make every path identical, which is the
    // failure this guards: the fast path is about chain variables, and a call with
    // none of those can still differ per iteration.
    assert!(sent.iter().collect::<std::collections::BTreeSet<_>>().len() == sent.len());
}
