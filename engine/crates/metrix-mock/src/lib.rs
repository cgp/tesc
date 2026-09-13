//! A standalone HTTP target with known, seeded injected behavior.

mod config;
mod model;

#[cfg(test)]
mod tests;

pub use config::{Config, ErrorInjection, Latency, SlowStart};

use std::{
    future::Future,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::Instant};

use model::{Decision, Model, Outcome};

/// Owns its listener and tasks so embedded tests can shut down without leaks.
pub struct MockServer {
    listener: TcpListener,
    state: Arc<State>,
    connections: Option<Arc<Semaphore>>,
}

struct State {
    model: Mutex<Model>,
    in_flight: Option<Arc<Semaphore>>,
}

impl MockServer {
    pub async fn bind(address: SocketAddr, config: Config) -> io::Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(address).await?;
        let connections = config.max_connections.map(|n| Arc::new(Semaphore::new(n)));
        let in_flight = config.max_in_flight.map(|n| Arc::new(Semaphore::new(n)));
        Ok(Self {
            listener,
            state: Arc::new(State {
                model: Mutex::new(Model::new(config, Instant::now())),
                in_flight,
            }),
            connections,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Shutdown cancels active requests, including injected timeouts.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        tokio::pin!(shutdown);
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    // Peer disconnects and injected faults are expected; task panics are not.
                    result.map_err(io::Error::other)?;
                }
                accepted = self.listener.accept() => {
                    let (socket, _) = accepted?;
                    let permit = if let Some(limit) = &self.connections {
                        match Arc::clone(limit).try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(_) => continue, // Drop the newly accepted socket without a response.
                        }
                    } else { None };
                    socket.set_nodelay(true)?;
                    let state = Arc::clone(&self.state);
                    tasks.spawn(async move {
                        let _permit = permit;
                        let service = service_fn(move |request| handle(request, Arc::clone(&state)));
                        let _ = Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(socket), service).await;
                    });
                }
            }
        }
        tasks.shutdown().await;
        Ok(())
    }
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<State>,
) -> io::Result<Response<Full<Bytes>>> {
    let _permit = if let Some(limit) = &state.in_flight {
        match Arc::clone(limit).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return Ok(response(503, "concurrency", Duration::ZERO)),
        }
    } else {
        None
    };
    // This target models a service, not the generator's lock-free hot path.
    // Never keep this lock across an await or body read.
    let decision = state
        .model
        .lock()
        .expect("model lock poisoned")
        .decide(Instant::now());
    let Some(Decision { delay, outcome }) = decision else {
        return Ok(response(503, "capacity", Duration::ZERO));
    };
    if outcome == Outcome::Disconnect {
        return Err(io::Error::other("injected disconnect"));
    }
    // Drain without retaining request bodies, allowing reuse for POST/PUT as well as GET.
    let mut body = request.into_body();
    while let Some(frame) = body.frame().await {
        frame.map_err(io::Error::other)?;
    }
    match outcome {
        Outcome::Timeout(duration) => {
            tokio::time::sleep(duration).await;
            Err(io::Error::other("injected timeout"))
        }
        Outcome::Http(status) => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(response(
                status,
                if status == 200 { "ok" } else { "http_error" },
                delay,
            ))
        }
        Outcome::Disconnect => unreachable!(),
    }
}

fn response(status: u16, reason: &'static str, delay: Duration) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-metrix-mock-outcome", reason)
        .header(
            "x-metrix-mock-delay-ms",
            format!("{:.6}", delay.as_secs_f64() * 1000.0),
        )
        .body(Full::new(Bytes::from_static(if status == 200 {
            b"{\"ok\":true}\n"
        } else {
            b"{\"ok\":false}\n"
        })))
        .expect("validated status and static response headers")
}
