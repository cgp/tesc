use super::*;
use hyper::{Response, service::service_fn};
use hyper_util::server::conn::auto::Builder;
use metrix_metrics::aggregation::Accumulator;
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use std::future::pending;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::ReusableBoxFuture;

struct TlsTarget {
    address: SocketAddr,
    roots: RootCertStore,
    task: JoinHandle<()>,
}
impl Drop for TlsTarget {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TlsTarget {
    async fn start(alpn: &[&[u8]]) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
                )
                .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let acceptor = acceptor.clone();
                        connections.spawn(async move {
                            let Ok(stream) = acceptor.accept(socket).await else { return; };
                            let service = service_fn(|request: hyper::Request<hyper::body::Incoming>| async move {
                                // Real request data crosses the TLS and framing path intact.
                                assert_eq!(request.uri().path(), "/echo");
                                let body = request.into_body().collect().await.unwrap().to_bytes();
                                Ok::<_, std::convert::Infallible>(Response::new(Full::new(body)))
                            });
                            let _ = Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(stream), service).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            address,
            roots,
            task,
        }
    }

    fn endpoint(&self, version: HttpVersion, trusted: bool, name: &str) -> Endpoint {
        Endpoint {
            addresses: vec![self.address],
            name: ServerName::try_from(name.to_owned()).unwrap(),
            version,
            tls: Some(Arc::new(tls_config(
                if trusted {
                    self.roots.clone()
                } else {
                    RootCertStore::empty()
                },
                version,
            ))),
        }
    }
}

fn template(uri: &str, timeout: Duration) -> Arc<RequestTemplate> {
    let uri: hyper::Uri = uri.parse().unwrap();
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::HOST,
        uri.authority().unwrap().as_str().parse().unwrap(),
    );
    Arc::new(RequestTemplate {
        method: hyper::Method::POST,
        uri,
        headers,
        body: Bytes::from_static(b"echo body"),
        timeout,
    })
}

#[tokio::test]
async fn tls_handshake_wait_is_queued_until_a_pre_send_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut hello = [0; 1024];
        assert!(socket.read(&mut hello).await.unwrap() > 0);
        pending::<()>().await;
        drop(socket);
    });
    let endpoint = Arc::new(Endpoint {
        addresses: vec![address],
        name: ServerName::try_from("localhost").unwrap(),
        tls: Some(Arc::new(tls_config(
            RootCertStore::empty(),
            HttpVersion::Http1,
        ))),
        version: HttpVersion::Http1,
    });
    let now = Instant::now();
    let state = Arc::new(SendState::default());
    let mut slots = vec![crate::Slot {
        active: true,
        worker: 0,
        iteration: 0,
        admitted: now,
        send_state: Arc::clone(&state),
        send_recorded: false,
        future: ReusableBoxFuture::new(execute(Some(Job {
            lease: Lease {
                connection: None,
                replacement: false,
            },
            endpoint,
            template: template(
                &format!("https://{address}/echo"),
                Duration::from_millis(500),
            ),
            scheduled: now,
            admitted: now,
            send_state: state,
        }))),
    }];
    assert!(
        timeout(Duration::from_millis(30), slots[0].future.get_pin())
            .await
            .is_err()
    );
    let mut workers = vec![Accumulator::default()];
    let mut interval = Accumulator::default();
    let mut report = crate::Report::default();
    let mut lag = crate::Lag::default();
    let mut last = Duration::ZERO;
    crate::flush(
        crate::Recording {
            slots: &mut slots,
            workers: &mut workers,
            lag: &mut lag,
        },
        &mut interval,
        &mut report,
        &mut last,
        now.elapsed(),
        1,
        None,
    );
    let window = report.last_window.as_ref().unwrap();
    assert_eq!(window.in_flight, 1);
    assert_eq!(window.queue_depth, 1);
    assert_eq!(window.metrics.drift.count(), 0);
    let completion = slots[0].future.get_pin().await;
    assert_eq!(completion.observation.error, Some(Failure::Timeout));
    assert!(completion.observation.sent.is_none());
    slots[0].active = false;
    crate::flush(
        crate::Recording {
            slots: &mut slots,
            workers: &mut workers,
            lag: &mut lag,
        },
        &mut interval,
        &mut report,
        &mut last,
        now.elapsed(),
        0,
        None,
    );
    assert_eq!(report.last_window.unwrap().queue_depth, 0);
    assert_eq!(report.sent, 0);
    server.abort();
}

#[tokio::test]
async fn verified_tls_negotiates_both_protocols_and_reuses_connections() {
    let server = TlsTarget::start(&[b"h2", b"http/1.1"]).await;
    for (version, expected) in [
        (HttpVersion::Auto, HttpVersion::Http2),
        (HttpVersion::Http1, HttpVersion::Http1),
        (HttpVersion::Http2, HttpVersion::Http2),
    ] {
        let endpoint = server.endpoint(version, true, "localhost");
        let mut connection = timeout(Duration::from_secs(5), endpoint.connect())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(connection.version(), expected);
        for _ in 0..2 {
            let mut observation = Observation::default();
            timeout(
                Duration::from_secs(5),
                exchange(
                    &mut connection,
                    &template("https://localhost/echo", Duration::from_secs(1)),
                    Instant::now(),
                    &SendState::default(),
                    &mut observation,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(observation.status, Some(200));
            assert_eq!(observation.bytes_received, 9);
            assert!(observation.ttfb.is_some());
        }
    }
}

#[tokio::test]
async fn tls_rejects_untrusted_certificates_wrong_names_and_protocol_mismatch() {
    let server = TlsTarget::start(&[b"http/1.1"]).await;
    for (version, trusted, name) in [
        (HttpVersion::Http1, false, "localhost"),
        (HttpVersion::Http1, true, "wrong.example"),
        (HttpVersion::Http2, true, "localhost"),
    ] {
        let result = timeout(
            Duration::from_secs(5),
            server.endpoint(version, trusted, name).connect(),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Failure::Tls | Failure::Protocol)));
    }
    // The portable CLI trust store also refuses this test's self-signed certificate.
    let target: Target = serde_json::from_value(serde_json::json!({"id": "tls", "address": server.address.to_string(), "tls": {"enabled": true}})).unwrap();
    assert!(matches!(
        Pool::prepare(&target, 1, Duration::from_secs(2)).await,
        Err(Failure::Tls)
    ));
}

#[tokio::test]
async fn timeout_includes_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        pending::<()>().await;
    });
    let target: Target = serde_json::from_value(
        serde_json::json!({"id": "tls", "address": address.to_string(), "tls": {"enabled": true}}),
    )
    .unwrap();
    assert!(matches!(
        Pool::prepare(&target, 1, Duration::from_millis(30)).await,
        Err(Failure::Timeout)
    ));
    task.abort();
}

#[tokio::test]
async fn truncated_and_stalled_response_bodies_are_failures_and_close_http1() {
    for stall in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            assert!(socket.read(&mut buf).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nabc")
                .await
                .unwrap();
            if stall {
                pending::<()>().await;
            }
        });
        let target: Target = serde_json::from_value(
            serde_json::json!({"id": "body", "address": address.to_string()}),
        )
        .unwrap();
        let mut pool = Pool::prepare(&target, 1, Duration::from_secs(1))
            .await
            .unwrap();
        let now = Instant::now();
        let completion = execute(Some(Job {
            lease: pool.acquire().unwrap(),
            endpoint: Arc::clone(&pool.endpoint),
            template: template(&format!("http://{address}/"), Duration::from_millis(100)),
            scheduled: now,
            admitted: now,
            send_state: Arc::new(SendState::default()),
        }))
        .await;
        assert_eq!(
            completion.observation.error,
            Some(if stall {
                Failure::Timeout
            } else {
                Failure::Body
            })
        );
        assert_eq!(completion.observation.status, Some(200));
        assert!(completion.lease.connection.is_none());
        pool.release(completion.lease);
        assert_eq!(pool.allocated, 0);
        task.abort();
    }
}
