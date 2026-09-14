mod support;
use http_body_util::Full;
use hyper::{Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, run};
use serde_json::json;
use std::{convert::Infallible, future::pending};

#[tokio::test]
async fn host_override_reaches_http1_and_http2_without_changing_socket_destination() {
    for version in ["http1", "http2"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let service = service_fn(
                        |request: hyper::Request<hyper::body::Incoming>| async move {
                            let host = request
                                .headers()
                                .get("host")
                                .and_then(|v| v.to_str().ok())
                                .or_else(|| request.uri().authority().map(|v| v.as_str()));
                            let status = if host == Some("api.example:8443") {
                                200
                            } else {
                                421
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            )
                        },
                    );
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), address, version);
        support::edit(dir.path(), "targets.json", |d| {
            d["list"][0]["host_header"] = json!("api.example:8443")
        });
        let report = run(Plan::load(dir.path()).unwrap(), pending())
            .await
            .unwrap();
        assert!(report.responses > 0);
        assert_eq!(report.metrics.counters.statuses[421], 0);
        assert_eq!(report.metrics.counters.statuses[200], report.responses);
        task.abort();
    }
}
