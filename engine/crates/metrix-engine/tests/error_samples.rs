//! B3.10: the first few failures, kept in full, with the secrets taken out.

mod support;

use std::{convert::Infallible, fs, net::SocketAddr, path::Path};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_metrics::events::{ErrorClass, ErrorSample, Record};
use serde_json::json;
use support::{bundle, edit};
use tokio::net::TcpListener;
use tokio::process::Command;

/// `/bad` answers 500 with a body worth reading; `/ok` answers 200.
async fn serve() -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = service_fn(|request: Request<hyper::body::Incoming>| async move {
                    let path = request.uri().path().to_owned();
                    let response = if path.starts_with("/bad") {
                        Response::builder()
                            .status(500)
                            .header("content-type", "application/json")
                            .header("set-cookie", "sid=abc")
                            .body(Full::new(Bytes::from_static(
                                br#"{"error":"orders unavailable","trace":"t-9"}"#,
                            )))
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"ok")))
                            .unwrap()
                    };
                    Ok::<_, Infallible>(response)
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

/// Run the real binary, so the samples come off the events stream as written.
async fn run_engine(root: &Path, secret: Option<(&str, &str)>) -> Vec<ErrorSample> {
    let events = root.join("events.ndjson");
    let mut command = Command::new(env!("CARGO_BIN_EXE_metrix-engine"));
    command
        .arg("--plan")
        .arg(root)
        .arg("--events")
        .arg(&events)
        .kill_on_drop(true);
    if let Some((name, value)) = secret {
        command.env(name, value);
    }
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(&events)
        .unwrap()
        .lines()
        .filter_map(|line| match serde_json::from_str::<Record>(line) {
            Ok(Record::ErrorSample(sample)) => Some(sample),
            _ => None,
        })
        .collect()
}

fn failing(root: &Path, address: SocketAddr, error_samples: u32) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({"ping": {"method": "POST", "path": "/bad", "body": "{\"user\":\"ada\"}"}}),
    );
    edit(root, "mix.json", |doc| {
        doc["capture"] = json!({
            "error_samples": error_samples, "body_max_kb": 64,
            "redact": ["Authorization", "Set-Cookie"],
        });
        doc["load"]["rate"] = json!(30);
        doc["load"]["duration"] = json!("1s");
    });
}

#[tokio::test]
async fn the_first_few_failures_are_kept_in_full() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    failing(dir.path(), address, 3);
    let samples = run_engine(dir.path(), None).await;

    assert_eq!(samples.len(), 3, "kept {}", samples.len());
    // Numbered, so a reader can tell three kept out of thirty from three that
    // happened. After N, the class only increments a counter.
    assert_eq!(
        samples.iter().map(|s| s.ordinal).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let first = &samples[0];
    assert_eq!(first.class, ErrorClass::HttpStatus);
    assert_eq!(first.chain, "ping");
    assert_eq!(first.step, "get");
    // The request as sent, and the answer that explains the failure. A sample without
    // the body is a sample of the fact that something went wrong.
    assert_eq!(first.request.method, "POST");
    assert_eq!(first.request.target, "/bad");
    assert_eq!(first.request.body.as_deref(), Some("{\"user\":\"ada\"}"));
    let response = first.response.as_ref().expect("the answer");
    assert_eq!(response.status, 500);
    assert!(
        response
            .body
            .as_deref()
            .is_some_and(|body| body.contains("orders unavailable")),
        "{response:?}"
    );
}

#[tokio::test]
async fn a_named_header_never_reaches_the_file() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    failing(dir.path(), address, 1);
    edit(dir.path(), "calls/ping.json", |doc| {
        doc["ping"]["headers"] = json!({"Authorization": "Bearer PRIVATE"});
    });
    let samples = run_engine(dir.path(), None).await;

    let sample = &samples[0];
    assert_eq!(sample.request.headers["authorization"], "[redacted]");
    // The response's `Set-Cookie` too, because a session id is a credential.
    let response = sample.response.as_ref().unwrap();
    assert_eq!(response.headers["set-cookie"], "[redacted]");
    // And nothing else is hidden: a sample with every header taken out explains
    // nothing at all.
    assert_eq!(response.headers["content-type"], "application/json");
}

#[tokio::test]
async fn a_resolved_secret_never_reaches_the_file_wherever_it_appears() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    failing(dir.path(), address, 1);
    edit(dir.path(), "calls/ping.json", |doc| {
        // In the query and in the body, under names nobody put on the redact list.
        doc["ping"]["query"] = json!({"tenant": "{{ secret.TENANT }}"});
        doc["ping"]["body"] = json!("{\"who\":\"{{ secret.TENANT }}\"}");
    });
    let samples = run_engine(dir.path(), Some(("METRIX_SECRET_TENANT", "swordfish-42"))).await;

    let request = &samples[0].request;
    let written = format!("{request:?}");
    // Redaction that only covered the names somebody remembered to list would be a
    // promise rather than a mechanism. The engine knows the literal values it
    // resolved, and takes them out wherever they turn up.
    assert!(!written.contains("swordfish-42"), "{written}");
    assert!(written.contains("[redacted]"), "{written}");
}

#[tokio::test]
async fn keeping_none_is_a_setting_the_run_honours() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    failing(dir.path(), address, 0);
    assert!(run_engine(dir.path(), None).await.is_empty());
}

#[tokio::test]
async fn a_step_that_never_sent_anything_is_sampled_by_what_it_wanted() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({
            "first": {"method": "GET", "path": "/ok", "extract": {"id": {"json": "$.id"}}},
            "second": {"method": "GET", "path": "/ok/{{ id }}"},
        }),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["chains"] = json!([{
            "name": "flow", "percent": 100, "session": "fresh",
            "steps": [{"id": "first", "call": "first"}, {"id": "second", "call": "second"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
    let samples = run_engine(dir.path(), None).await;

    let broken = samples
        .iter()
        .find(|sample| sample.class == ErrorClass::Extraction)
        .expect("the chain broke and nothing said why");
    // Nothing reached the wire, so there is no request or response to keep. Naming
    // the variable is the whole sample, and it is the difference between a plan error
    // and a service that started returning 404s.
    assert_eq!(broken.step, "second");
    assert_eq!(broken.detail.as_deref(), Some("id"));
    assert!(broken.response.is_none());
}
