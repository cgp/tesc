use super::*;
use hyper::client::conn::{http1, http2};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    time::timeout,
};

const DEADLINE: Duration = Duration::from_secs(5);

struct Running {
    address: SocketAddr,
    state: Arc<State>,
    connections: Option<Arc<Semaphore>>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl Running {
    async fn start(config: Config) -> Self {
        let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), config)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let state = Arc::clone(&server.state);
        let connections = server.connections.clone();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run_until(async {
            let _ = stopped.await;
        }));
        Self {
            address,
            state,
            connections,
            stop: Some(stop),
            task,
        }
    }

    async fn http1(&self) -> http1::SendRequest<Full<Bytes>> {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let (sender, connection) = http1::handshake(TokioIo::new(stream)).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        sender
    }

    async fn http2(&self) -> http2::SendRequest<Full<Bytes>> {
        let stream = TcpStream::connect(self.address).await.unwrap();
        let (sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        sender
    }

    async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(DEADLINE, &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request() -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri("http://localhost/arbitrary?query=1")
        .header("host", "localhost")
        .body(Full::new(Bytes::from_static(b"request body")))
        .unwrap()
}

async fn wait_for_permits(semaphore: &Semaphore, permits: usize) {
    timeout(DEADLINE, async {
        while semaphore.available_permits() != permits {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn http1_drains_bodies_and_reuses_connection_with_injected_delay() {
    let server = Running::start(Config {
        latency: Latency::Fixed { ms: 30.0 },
        ..Config::default()
    })
    .await;
    let mut client = server.http1().await;
    for _ in 0..3 {
        let start = Instant::now();
        let response = timeout(DEADLINE, client.send_request(request()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), hyper::Version::HTTP_11);
        assert_eq!(response.headers()["x-metrix-mock-delay-ms"], "30.000000");
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "{\"ok\":true}\n"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn http2_multiplexing_respects_request_concurrency_and_recovers() {
    let server = Running::start(Config {
        latency: Latency::Fixed { ms: 100.0 },
        max_in_flight: Some(1),
        max_connections: Some(1),
        ..Config::default()
    })
    .await;
    let mut client = server.http2().await;
    let first = client.send_request(request());
    let first = tokio::spawn(first);
    wait_for_permits(server.state.in_flight.as_ref().unwrap(), 0).await;
    let rejected = timeout(DEADLINE, client.send_request(request()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status(), 503);
    assert_eq!(rejected.headers()["x-metrix-mock-outcome"], "concurrency");
    assert_eq!(rejected.version(), hyper::Version::HTTP_2);
    let response = timeout(DEADLINE, first).await.unwrap().unwrap().unwrap();
    assert_eq!(response.status(), 200);
    response.into_body().collect().await.unwrap();
    let recovered = timeout(DEADLINE, client.send_request(request()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.status(), 200);
    server.stop().await;
}

#[tokio::test]
async fn capacity_returns_immediate_503_without_latency() {
    let server = Running::start(Config {
        capacity_rps: Some(1),
        latency: Latency::Fixed { ms: 0.0 },
        ..Config::default()
    })
    .await;
    let mut client = server.http1().await;
    let first = client.send_request(request()).await.unwrap();
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();
    // Refill arithmetic is checked separately with synthetic instants. These two
    // zero-delay loopback requests exercise the server's overload response wiring.
    let second = timeout(DEADLINE, client.send_request(request()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.status(), 503);
    assert_eq!(second.headers()["x-metrix-mock-outcome"], "capacity");
    assert_eq!(second.headers()["x-metrix-mock-delay-ms"], "0.000000");
    server.stop().await;
}

#[tokio::test]
async fn http_errors_apply_delay_and_preserve_keep_alive_on_both_protocols() {
    let config = Config {
        latency: Latency::Fixed { ms: 5.0 },
        errors: vec![ErrorInjection::Http {
            rate: 1.0,
            status: 429,
        }],
        ..Config::default()
    };
    let server = Running::start(config).await;
    let mut http1 = server.http1().await;
    let mut http2 = server.http2().await;
    for _ in 0..2 {
        for response in [
            timeout(DEADLINE, http1.send_request(request()))
                .await
                .unwrap()
                .unwrap(),
            timeout(DEADLINE, http2.send_request(request()))
                .await
                .unwrap()
                .unwrap(),
        ] {
            assert_eq!(response.status(), 429);
            assert_eq!(response.headers()["x-metrix-mock-outcome"], "http_error");
            assert_eq!(response.headers()["x-metrix-mock-delay-ms"], "5.000000");
            response.into_body().collect().await.unwrap();
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn disconnects_and_timeouts_produce_transport_errors_on_both_protocols() {
    for error in [
        ErrorInjection::Disconnect { rate: 1.0 },
        ErrorInjection::Timeout {
            rate: 1.0,
            delay_ms: 50.0,
        },
    ] {
        let expected_wait = if matches!(error, ErrorInjection::Timeout { .. }) {
            Duration::from_millis(50)
        } else {
            Duration::ZERO
        };
        let server = Running::start(Config {
            errors: vec![error],
            ..Config::default()
        })
        .await;
        let mut http1 = server.http1().await;
        let start = Instant::now();
        assert!(
            timeout(DEADLINE, http1.send_request(request()))
                .await
                .unwrap()
                .is_err()
        );
        assert!(start.elapsed() >= expected_wait);
        let mut http2 = server.http2().await;
        for _ in 0..2 {
            let start = Instant::now();
            assert!(
                timeout(DEADLINE, http2.send_request(request()))
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(start.elapsed() >= expected_wait);
        }
        server.stop().await;
    }
}

#[tokio::test]
async fn http2_recovers_on_the_same_connection_after_injected_stream_faults() {
    for fault in [
        ErrorInjection::Disconnect { rate: 0.5 },
        ErrorInjection::Timeout {
            rate: 0.5,
            delay_ms: 1.0,
        },
    ] {
        let server = Running::start(Config {
            errors: vec![fault],
            latency: Latency::Fixed { ms: 0.0 },
            ..Config::default()
        })
        .await;
        let mut client = server.http2().await;
        let mut saw_error = false;
        let mut recovered = false;
        for _ in 0..20 {
            match timeout(DEADLINE, client.send_request(request()))
                .await
                .unwrap()
            {
                Ok(response) => {
                    assert_eq!(response.status(), 200);
                    response.into_body().collect().await.unwrap();
                    recovered |= saw_error;
                }
                Err(_) => saw_error = true,
            }
        }
        assert!(saw_error && recovered);
        server.stop().await;
    }
}

#[tokio::test]
async fn http2_shutdown_cancels_active_streams() {
    let server = Running::start(Config {
        max_in_flight: Some(1),
        errors: vec![ErrorInjection::Timeout {
            rate: 1.0,
            delay_ms: 3_600_000.0,
        }],
        ..Config::default()
    })
    .await;
    let state = Arc::clone(&server.state);
    let mut client = server.http2().await;
    let response = tokio::spawn(async move { client.send_request(request()).await });
    wait_for_permits(state.in_flight.as_ref().unwrap(), 0).await;
    server.stop().await;
    assert!(timeout(DEADLINE, response).await.unwrap().unwrap().is_err());
    wait_for_permits(state.in_flight.as_ref().unwrap(), 1).await;
}

#[tokio::test]
async fn connection_ceiling_closes_excess_sockets_then_accepts_after_release() {
    let server = Running::start(Config {
        max_connections: Some(1),
        ..Config::default()
    })
    .await;
    let first = server.http1().await;
    wait_for_permits(server.connections.as_ref().unwrap(), 0).await;
    let mut excess = TcpStream::connect(server.address).await.unwrap();
    let mut byte = [0];
    let result = timeout(DEADLINE, excess.read(&mut byte)).await.unwrap();
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "excess connection must close"
    );
    drop(first);
    wait_for_permits(server.connections.as_ref().unwrap(), 1).await;
    let mut recovered = server.http1().await;
    assert_eq!(
        timeout(DEADLINE, recovered.send_request(request()))
            .await
            .unwrap()
            .unwrap()
            .status(),
        200
    );
    server.stop().await;
}

#[tokio::test]
async fn cancelled_requests_release_capacity_and_shutdown_interrupts_timeouts() {
    let server = Running::start(Config {
        max_in_flight: Some(1),
        errors: vec![ErrorInjection::Timeout {
            rate: 1.0,
            delay_ms: 3_600_000.0,
        }],
        ..Config::default()
    })
    .await;
    // An incomplete body holds a request permit; closing the socket must release it.
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nx")
        .await
        .unwrap();
    wait_for_permits(server.state.in_flight.as_ref().unwrap(), 0).await;
    drop(socket);
    wait_for_permits(server.state.in_flight.as_ref().unwrap(), 1).await;
    let mut client = server.http1().await;
    let response = tokio::spawn(async move { client.send_request(request()).await });
    wait_for_permits(server.state.in_flight.as_ref().unwrap(), 0).await;
    let address = server.address;
    server.stop().await;
    assert!(timeout(DEADLINE, response).await.unwrap().unwrap().is_err());
    assert!(TcpStream::connect(address).await.is_err());
}
