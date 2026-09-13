//! Hyper connections and rustls directly; no protocol abstraction or ambient proxy config.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request,
    client::conn::{http1, http2},
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_plan::{Target, targets::HttpVersion};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use tokio_rustls::TlsConnector;

use crate::bundle::RequestTemplate;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Dns,
    Connect,
    Tls,
    Protocol,
    Send,
    Body,
    Timeout,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub(crate) struct Endpoint {
    addresses: Vec<SocketAddr>,
    name: ServerName<'static>,
    tls: Option<Arc<ClientConfig>>,
    version: HttpVersion,
}

impl Endpoint {
    async fn resolve(target: &Target) -> Result<Self, Failure> {
        let authority = target
            .address
            .parse::<hyper::http::uri::Authority>()
            .map_err(|_| Failure::Connect)?;
        let host = authority.host().trim_matches(['[', ']']);
        let name = ServerName::try_from(host.to_owned()).map_err(|_| Failure::Tls)?;
        let addresses: Vec<_> =
            tokio::net::lookup_host((host, authority.port_u16().ok_or(Failure::Connect)?))
                .await
                .map_err(|_| Failure::Dns)?
                .collect();
        if addresses.is_empty() {
            return Err(Failure::Dns);
        }
        let tls = target.tls.enabled.then(|| {
            let roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            Arc::new(tls_config(roots, target.http_version))
        });
        Ok(Self {
            addresses,
            name,
            tls,
            version: target.http_version,
        })
    }

    async fn connect(&self) -> Result<Connection, Failure> {
        let socket = TcpStream::connect(self.addresses.as_slice())
            .await
            .map_err(|_| Failure::Connect)?;
        socket.set_nodelay(true).map_err(|_| Failure::Connect)?;
        if let Some(config) = &self.tls {
            let stream = TlsConnector::from(Arc::clone(config))
                .connect(self.name.clone(), socket)
                .await
                .map_err(|_| Failure::Tls)?;
            let negotiated = match stream.get_ref().1.alpn_protocol() {
                Some(b"h2") => HttpVersion::Http2,
                Some(b"http/1.1") | None => HttpVersion::Http1,
                _ => return Err(Failure::Protocol),
            };
            if self.version != HttpVersion::Auto && self.version != negotiated {
                return Err(Failure::Protocol);
            }
            handshake(stream, negotiated).await
        } else {
            handshake(socket, self.version).await
        }
    }
}

fn tls_config(roots: RootCertStore, version: HttpVersion) -> ClientConfig {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = match version {
        HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpVersion::Http1 => vec![b"http/1.1".to_vec()],
        HttpVersion::Http2 => vec![b"h2".to_vec()],
    };
    config
}

pub(crate) struct Driver(JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) enum Connection {
    Http1 {
        sender: http1::SendRequest<Full<Bytes>>,
        _driver: Arc<Driver>,
        used: bool,
    },
    Http2 {
        sender: http2::SendRequest<Full<Bytes>>,
        _driver: Arc<Driver>,
        used: bool,
    },
}

impl Connection {
    fn version(&self) -> HttpVersion {
        match self {
            Self::Http1 { .. } => HttpVersion::Http1,
            Self::Http2 { .. } => HttpVersion::Http2,
        }
    }
    fn is_closed(&self) -> bool {
        match self {
            Self::Http1 { sender, .. } => sender.is_closed(),
            Self::Http2 { sender, .. } => sender.is_closed(),
        }
    }
    fn multiplex(&mut self) -> Self {
        match self {
            Self::Http2 {
                sender,
                _driver,
                used,
            } => {
                let previous = *used;
                *used = true;
                Self::Http2 {
                    sender: sender.clone(),
                    _driver: Arc::clone(_driver),
                    used: previous,
                }
            }
            Self::Http1 { .. } => unreachable!("HTTP/1.1 connections are checked out exclusively"),
        }
    }
}

async fn handshake<T>(stream: T, version: HttpVersion) -> Result<Connection, Failure>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if version == HttpVersion::Http2 {
        let (sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .map_err(|_| Failure::Protocol)?;
        Ok(Connection::Http2 {
            sender,
            used: false,
            _driver: Arc::new(Driver(tokio::spawn(async {
                let _ = connection.await;
            }))),
        })
    } else {
        let (sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| Failure::Protocol)?;
        Ok(Connection::Http1 {
            sender,
            used: false,
            _driver: Arc::new(Driver(tokio::spawn(async {
                let _ = connection.await;
            }))),
        })
    }
}

pub(crate) struct Pool {
    pub endpoint: Arc<Endpoint>,
    idle: Vec<Connection>,
    shared: Option<Connection>,
    version: HttpVersion,
    allocated: usize,
    max: usize,
    reconnecting: bool,
}

pub(crate) struct Lease {
    connection: Option<Connection>,
    replacement: bool,
}

impl Pool {
    pub async fn prepare(target: &Target, max: usize, deadline: Duration) -> Result<Self, Failure> {
        timeout(deadline, async {
            let mut endpoint = Endpoint::resolve(target).await?;
            let first = endpoint.connect().await?;
            endpoint.version = first.version();
            let mut idle = Vec::new();
            // Setup allocation only. OOM is reported rather than reserving on admission.
            idle.try_reserve_exact(max).map_err(|_| Failure::Connect)?;
            let version = first.version();
            let shared = if version == HttpVersion::Http2 {
                Some(first)
            } else {
                idle.push(first);
                None
            };
            Ok(Self {
                endpoint: Arc::new(endpoint),
                idle,
                shared,
                version,
                allocated: 1,
                max,
                reconnecting: false,
            })
        })
        .await
        .map_err(|_| Failure::Timeout)?
    }

    pub fn acquire(&mut self) -> Option<Lease> {
        if self.version == HttpVersion::Http2 {
            if let Some(shared) = &mut self.shared {
                if !shared.is_closed() {
                    return Some(Lease {
                        connection: Some(shared.multiplex()),
                        replacement: false,
                    });
                }
            }
            if self.reconnecting {
                return None;
            }
            self.shared = None;
            self.reconnecting = true;
            return Some(Lease {
                connection: None,
                replacement: true,
            });
        }
        if let Some(connection) = self.idle.pop() {
            Some(Lease {
                connection: Some(connection),
                replacement: false,
            })
        } else if self.allocated < self.max {
            self.allocated += 1;
            Some(Lease {
                connection: None,
                replacement: false,
            })
        } else {
            None
        }
    }

    pub fn release(&mut self, lease: Lease) {
        if self.version == HttpVersion::Http2 {
            if lease.replacement {
                self.reconnecting = false;
                self.shared = lease.connection;
            }
        } else if let Some(connection) = lease.connection {
            self.idle.push(connection);
        } else {
            self.allocated -= 1;
        }
    }
}

pub(crate) struct Job {
    pub lease: Lease,
    pub endpoint: Arc<Endpoint>,
    pub template: Arc<RequestTemplate>,
    pub scheduled: Instant,
    pub admitted: Instant,
    pub send_state: Arc<SendState>,
}

/// Allocated once per reusable slot. Zero means not sent; one encodes zero drift.
#[derive(Default)]
pub(crate) struct SendState(AtomicU64);

impl SendState {
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
    pub fn drift(&self) -> Option<Duration> {
        let value = self.0.load(Ordering::Relaxed);
        (value != 0).then(|| Duration::from_nanos(value - 1))
    }
    fn record(&self, drift: Duration) {
        let encoded = drift.as_nanos().min(u128::from(u64::MAX - 1)) as u64 + 1;
        self.0.store(encoded, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Observation {
    pub sent: Option<Instant>,
    pub drift: Duration,
    pub total: Duration,
    pub ttfb: Option<Duration>,
    pub status: Option<u16>,
    pub bytes_received: u64,
    pub error: Option<Failure>,
    pub request_duration: Option<Duration>,
    pub connections_opened: u64,
    pub connection_reused: bool,
}

pub(crate) struct Completion {
    pub lease: Lease,
    pub observation: Observation,
}

/// One uniform future type lets every scheduler slot reuse its allocation.
pub(crate) async fn execute(job: Option<Job>) -> Completion {
    let Some(mut job) = job else {
        return std::future::pending().await;
    };
    let mut observation = Observation::default();
    let deadline = job.admitted + job.template.timeout;
    let result = timeout_at(deadline, async {
        if job
            .lease
            .connection
            .as_ref()
            .is_none_or(Connection::is_closed)
        {
            job.lease.connection = None; // Close stale socket before opening its replacement.
            job.lease.connection = Some(job.endpoint.connect().await?);
            observation.connections_opened += 1;
        }
        exchange(
            job.lease.connection.as_mut().expect("connected"),
            &job.template,
            job.scheduled,
            &job.send_state,
            &mut observation,
        )
        .await
    })
    .await
    .unwrap_or(Err(Failure::Timeout));
    observation.total = job.admitted.elapsed();
    observation.request_duration = observation.sent.map(|sent| sent.elapsed());
    observation.error = result.err();
    if observation.error.is_some()
        && job
            .lease
            .connection
            .as_ref()
            .is_some_and(|c| c.version() == HttpVersion::Http1 || c.is_closed())
    {
        job.lease.connection = None;
    }
    Completion {
        lease: job.lease,
        observation,
    }
}

async fn exchange(
    connection: &mut Connection,
    template: &RequestTemplate,
    scheduled: Instant,
    send_state: &SendState,
    observation: &mut Observation,
) -> Result<(), Failure> {
    let mut request = Request::new(Full::new(template.body.clone()));
    *request.method_mut() = template.method.clone();
    *request.headers_mut() = template.headers.clone();
    let response = match connection {
        Connection::Http1 { sender, used, .. } => {
            *request.uri_mut() = UriPath::origin(&template.uri);
            sender.ready().await.map_err(|_| Failure::Send)?;
            mark_sent(observation, scheduled, send_state);
            observation.connection_reused = *used;
            *used = true;
            sender
                .send_request(request)
                .await
                .map_err(|_| Failure::Send)?
        }
        Connection::Http2 { sender, used, .. } => {
            *request.uri_mut() = template.uri.clone();
            *request.version_mut() = hyper::Version::HTTP_2;
            sender.ready().await.map_err(|_| Failure::Send)?;
            mark_sent(observation, scheduled, send_state);
            observation.connection_reused = *used;
            *used = true;
            sender
                .send_request(request)
                .await
                .map_err(|_| Failure::Send)?
        }
    };
    observation.ttfb = observation.sent.map(|sent| sent.elapsed());
    observation.status = Some(response.status().as_u16());
    let mut body = response.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Failure::Body)?;
        if let Some(data) = frame.data_ref() {
            observation.bytes_received += data.len() as u64;
        }
    }
    Ok(())
}

fn mark_sent(observation: &mut Observation, scheduled: Instant, send_state: &SendState) {
    let sent = Instant::now();
    observation.sent = Some(sent);
    observation.drift = sent.saturating_duration_since(scheduled);
    send_state.record(observation.drift);
}

// Keep origin-form construction out of request string formatting.
struct UriPath;
impl UriPath {
    fn origin(uri: &hyper::Uri) -> hyper::Uri {
        hyper::Uri::builder()
            .path_and_query(
                uri.path_and_query()
                    .expect("compiled URI has a path")
                    .clone(),
            )
            .build()
            .expect("compiled path")
    }
}
