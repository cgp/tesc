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

use crate::calls::{Prepared, RequestTemplate};

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
        let host_name = target
            .host_header
            .as_deref()
            .unwrap_or(host)
            .parse::<hyper::http::uri::Authority>()
            .map_err(|_| Failure::Tls)?;
        let tls_name = target
            .tls
            .sni
            .as_deref()
            .unwrap_or(host_name.host())
            .trim_matches(['[', ']']);
        let name = ServerName::try_from(tls_name.to_owned()).map_err(|_| Failure::Tls)?;
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
            let mut config = tls_config(roots, target.http_version);
            if target.tls.insecure_skip_verify {
                config
                    .dangerous()
                    .set_certificate_verifier(Arc::new(SkipCertificateVerification));
            }
            Arc::new(config)
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

/// Allocated once per reusable slot. Zero means not sent; one encodes zero drift.
#[derive(Default)]
pub(crate) struct SendState {
    drift: AtomicU64,
    /// The error-sample budget, so a failed answer's body is kept only while
    /// somebody still wants one. `None` for the auth transport, whose round trips are
    /// not part of the run being measured.
    pub samples: Option<Arc<crate::samples::Samples>>,
}

impl SendState {
    /// A slot's state, holding the run's sample budget so an errored answer can be
    /// kept without the send path reaching for anything global.
    pub fn for_run(samples: Arc<crate::samples::Samples>) -> Self {
        Self {
            drift: AtomicU64::new(0),
            samples: Some(samples),
        }
    }

    pub fn reset(&self) {
        self.drift.store(0, Ordering::Relaxed);
    }
    pub fn drift(&self) -> Option<Duration> {
        let value = self.drift.load(Ordering::Relaxed);
        (value != 0).then(|| Duration::from_nanos(value - 1))
    }
    fn record(&self, drift: Duration) {
        let encoded = drift.as_nanos().min(u128::from(u64::MAX - 1)) as u64 + 1;
        self.drift.store(encoded, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(crate) struct Observation {
    pub sent: Option<Instant>,
    pub drift: Duration,
    pub admission_delay: Duration,
    pub total: Duration,
    pub ttfb: Option<Duration>,
    pub status: Option<u16>,
    pub bytes_received: u64,
    pub error: Option<Failure>,
    pub request_duration: Option<Duration>,
    pub connections_opened: u64,
    pub connection_reused: bool,
    pub bytes_sent: u64,
    /// The response, kept only when a call reads it. A load generator that buffers
    /// every body it receives is measuring its own allocator as much as the service.
    pub response: Option<CapturedResponse>,
}

/// What a chain needs to read a value out of a response.
pub(crate) struct CapturedResponse {
    pub headers: hyper::HeaderMap,
    pub body: Bytes,
    /// True when the body was longer than the capture ceiling and was cut. An
    /// extractor reading a truncated document will simply not match, and saying the
    /// body was cut is the difference between that and a service that stopped
    /// sending the field.
    pub truncated: bool,
}

/// When this request was meant to go, when it was admitted, and whether it is the
/// one the schedule is measured against.
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub scheduled: Instant,
    pub admitted: Instant,
    pub records_drift: bool,
}

/// Send one request on a held lease and observe what came back.
///
/// The lease is borrowed rather than consumed, because a chain sends several
/// requests down one connection: the steps of an iteration are one virtual user, and
/// giving each of them its own socket would measure a service being connected to
/// rather than a service being used.
pub(crate) async fn send(
    lease: &mut Lease,
    endpoint: &Endpoint,
    template: &RequestTemplate,
    rendered: Option<&Prepared>,
    timing: Timing,
    send_state: &SendState,
) -> Observation {
    let prepared = rendered
        .or_else(|| template.prepared())
        .expect("a call is either fixed or rendered");
    let mut observation = Observation {
        bytes_sent: prepared.body.len() as u64,
        ..Observation::default()
    };
    let deadline = timing.admitted + template.timeout;
    let result = timeout_at(deadline, async {
        if lease.connection.as_ref().is_none_or(Connection::is_closed) {
            lease.connection = None; // Close stale socket before opening its replacement.
            lease.connection = Some(endpoint.connect().await?);
            observation.connections_opened += 1;
        }
        exchange(
            lease.connection.as_mut().expect("connected"),
            template,
            prepared,
            timing,
            send_state,
            &mut observation,
        )
        .await
    })
    .await
    .unwrap_or(Err(Failure::Timeout));
    let finished = Instant::now();
    observation.total = finished.saturating_duration_since(timing.admitted);
    observation.request_duration = observation
        .sent
        .map(|sent| finished.saturating_duration_since(sent));
    observation.admission_delay = timing.admitted.saturating_duration_since(timing.scheduled);
    observation.error = result.err();
    if observation.error.is_some()
        && lease
            .connection
            .as_ref()
            .is_some_and(|c| c.version() == HttpVersion::Http1 || c.is_closed())
    {
        lease.connection = None;
    }
    observation
}

async fn exchange(
    connection: &mut Connection,
    template: &RequestTemplate,
    prepared: &Prepared,
    timing: Timing,
    send_state: &SendState,
    observation: &mut Observation,
) -> Result<(), Failure> {
    let mut request = Request::new(Full::new(prepared.body.clone()));
    *request.method_mut() = template.method.clone();
    *request.headers_mut() = prepared.headers.clone();
    let response = match connection {
        Connection::Http1 { sender, used, .. } => {
            *request.uri_mut() = UriPath::origin(&prepared.uri);
            sender.ready().await.map_err(|_| Failure::Send)?;
            mark_sent(observation, timing, send_state);
            observation.connection_reused = *used;
            *used = true;
            sender
                .send_request(request)
                .await
                .map_err(|_| Failure::Send)?
        }
        Connection::Http2 { sender, used, .. } => {
            *request.uri_mut() = prepared.uri.clone();
            *request.version_mut() = hyper::Version::HTTP_2;
            sender.ready().await.map_err(|_| Failure::Send)?;
            mark_sent(observation, timing, send_state);
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

    // An answer that failed is worth keeping in full while somebody is still
    // collecting them (§9.3). Decided here, from the status line, before a single
    // body frame has arrived — and only while a class still has budget, because a
    // broken run produces errors by the thousand and the ceiling exists so the
    // generator does not hold them all.
    let failed = observation.status.is_some_and(|status| status >= 400)
        && send_state
            .samples
            .as_ref()
            .is_some_and(|s| s.wants_bodies());
    let keep_headers = template.reads_headers() || failed;
    let headers = keep_headers.then(|| response.headers().clone());
    let mut kept = (template.reads_body() || failed)
        .then(|| Vec::with_capacity(template.body_ceiling().min(8 * 1024)));
    let ceiling = template.body_ceiling();
    let mut truncated = false;

    let mut body = response.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Failure::Body)?;
        if let Some(data) = frame.data_ref() {
            observation.bytes_received += data.len() as u64;
            // Counted whatever happens, kept only up to the ceiling: the byte count
            // is a measurement and the body is evidence, and running out of room for
            // the second must not change the first.
            if let Some(buffer) = kept.as_mut() {
                let room = ceiling.saturating_sub(buffer.len());
                if room == 0 {
                    truncated = true;
                } else if data.len() > room {
                    buffer.extend_from_slice(&data[..room]);
                    truncated = true;
                } else {
                    buffer.extend_from_slice(data);
                }
            }
        }
    }

    if keep_headers || kept.is_some() {
        observation.response = Some(CapturedResponse {
            headers: headers.unwrap_or_default(),
            body: Bytes::from(kept.unwrap_or_default()),
            truncated,
        });
    }
    Ok(())
}

fn mark_sent(observation: &mut Observation, timing: Timing, send_state: &SendState) {
    let sent = Instant::now();
    observation.sent = Some(sent);
    observation.drift = sent.saturating_duration_since(timing.scheduled);
    if timing.records_drift {
        // Only the first send of an iteration. A later step is late because the
        // service was slow, and folding that into send drift would report the
        // target's latency as the generator's lateness.
        send_state.record(observation.drift);
    }
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

/// Skip trust/name checks only; handshake signatures still prove key possession.
#[derive(Debug)]
struct SkipCertificateVerification;
impl rustls::client::danger::ServerCertVerifier for SkipCertificateVerification {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
