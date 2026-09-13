mod support;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use metrix_engine::{Plan, run};
use serde_json::json;
use std::{
    convert::Infallible,
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};

#[tokio::test]
async fn static_requests_merge_headers_encode_queries_and_drain_bodies_for_reuse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let connection_count = Arc::clone(&connections);
    let request_count = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        let response_body = Bytes::from(vec![b'x'; 256 * 1024]);
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    connection_count.fetch_add(1, Ordering::Relaxed);
                    let requests = Arc::clone(&request_count);
                    let body = response_body.clone();
                    tasks.spawn(async move {
                        let service = service_fn(move |request: hyper::Request<Incoming>| {
                            let requests = Arc::clone(&requests);
                            let body = body.clone();
                            async move {
                                assert_eq!(request.method(), hyper::Method::POST);
                                assert_eq!(request.uri().path_and_query().unwrap().as_str(), "/echo?base=1&search=snow%20%26%20%E2%98%83");
                                assert_eq!(request.headers()["x-mode"], "call");
                                assert_eq!(request.headers()["x-default"], "retained");
                                assert_eq!(request.into_body().collect().await.unwrap().to_bytes(), "payload");
                                requests.fetch_add(1, Ordering::Relaxed);
                                Ok::<_, Infallible>(Response::new(Full::new(body)))
                            }
                        });
                        Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(socket), service).await.unwrap();
                    });
                }
                Some(result) = tasks.join_next(), if !tasks.is_empty() => { result.unwrap(); }
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), address, "http1");
    support::edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(4);
        doc["engine"]["connections_per_host"] = json!(1);
        doc["defaults"]["headers"] = json!({"x-mode": "default", "x-default": "retained"});
    });
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"ping": {
            "method": "POST", "path": "/echo?base=1", "query": {"search": "snow & ☃"},
            "headers": {"X-Mode": "call"}, "body": "payload"
        }}),
    );
    let report = timeout(
        Duration::from_secs(5),
        run(Plan::load(dir.path()).unwrap(), pending()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.responses, 4, "{report:?}");
    assert_eq!(report.failed, 0);
    assert_eq!(connections.load(Ordering::Relaxed), 1);
    assert_eq!(requests.load(Ordering::Relaxed), 4);
    server.abort();
}
