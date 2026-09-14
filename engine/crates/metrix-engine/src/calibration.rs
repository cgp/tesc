//! Local generator calibration and the machine profile kept with a plan bundle.

use crate::Plan;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{PrivatePkcs8KeyDer, ServerName},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    hint::black_box,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub const PROFILE_FILE: &str = "machine-profile.json";
const PROFILE_VERSION: u32 = 1;
const WINDOW: Duration = Duration::from_millis(250);

/// A calibration result is tied to the generator and request shape, rather than a target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub version: u32,
    pub id: String,
    pub hardware: Hardware,
    pub shape: Shape,
    pub ceilings: Vec<Ceiling>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    pub architecture: String,
    pub logical_cores: usize,
    pub physical_cores: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    pub request_body_bytes: usize,
    pub tls: bool,
    pub chain_depth: u32,
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ceiling {
    pub worker_threads: usize,
    /// Executor-only ceiling. It isolates scheduler and request-build capacity.
    pub null_rps: f64,
    /// Conservative persistent-connection loopback echo ceiling.
    pub loopback_rps: f64,
}

impl Hardware {
    fn current() -> Self {
        Self {
            architecture: std::env::consts::ARCH.into(),
            logical_cores: num_cpus::get(),
            physical_cores: num_cpus::get_physical(),
        }
    }
}

impl MachineProfile {
    pub(crate) fn load(root: &Path, shape: &Shape, workers: usize) -> Result<Option<Self>, String> {
        let path = root.join(PROFILE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|_| "machine-profile.json: cannot read profile")?;
        let profile: Self =
            serde_json::from_slice(&bytes).map_err(|_| "machine-profile.json: invalid profile")?;
        profile.validate(shape, workers)?;
        Ok(Some(profile))
    }

    pub(crate) fn ceiling(&self, workers: usize) -> f64 {
        self.ceilings
            .iter()
            .find(|point| point.worker_threads == workers)
            .expect("validated profile has configured worker point")
            .loopback_rps
    }

    fn validate(&self, shape: &Shape, workers: usize) -> Result<(), String> {
        if self.version != PROFILE_VERSION
            || self.hardware != Hardware::current()
            || &self.shape != shape
            || self.id.is_empty()
            || self.ceilings.is_empty()
            || self.ceilings.iter().any(|point| {
                point.worker_threads == 0
                    || !point.null_rps.is_finite()
                    || !point.loopback_rps.is_finite()
                    || point.null_rps <= 0.0
                    || point.loopback_rps <= 0.0
            })
            || !self
                .ceilings
                .iter()
                .any(|point| point.worker_threads == workers)
        {
            return Err("machine-profile.json: profile does not match this machine, plan shape, or worker count; run --calibrate".into());
        }
        Ok(())
    }
}

/// Calibrate without touching the target. The profile is intentionally stored inside the bundle.
pub fn calibrate(plan: &Plan) -> Result<MachineProfile, String> {
    let workers = calibration_workers(plan.worker_threads);
    let mut ceilings = Vec::with_capacity(workers.len());
    for worker_threads in workers {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()
            .map_err(|_| "cannot create calibration runtime")?;
        let (null_rps, observed_loopback_rps) = runtime.block_on(async {
            let null = null_rps(worker_threads).await;
            let loopback = loopback_rps(
                worker_threads,
                plan.calibration_shape.request_body_bytes,
                plan.calibration_shape.tls,
            )
            .await?;
            Ok::<_, String>((null, loopback))
        })?;
        runtime.shutdown_timeout(Duration::from_millis(100));
        ceilings.push(Ceiling {
            worker_threads,
            null_rps,
            // Leave margin for target DNS, HTTP framing and the output path. A calibration
            // ceiling is a safety limit, so it must be lower than the observed loopback rate.
            loopback_rps: observed_loopback_rps * 0.9,
        });
    }
    let hardware = Hardware::current();
    let shape = plan.calibration_shape.clone();
    let id = profile_id(&hardware, &shape, &ceilings);
    Ok(MachineProfile {
        version: PROFILE_VERSION,
        id,
        hardware,
        shape,
        ceilings,
    })
}

pub fn write_profile(root: &Path, profile: &MachineProfile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(profile).expect("machine profiles serialize");
    fs::write(root.join(PROFILE_FILE), bytes)
        .map_err(|_| "machine-profile.json: cannot write profile".to_owned())
}

fn calibration_workers(configured: usize) -> Vec<usize> {
    if configured == 1 {
        vec![1]
    } else {
        vec![1, configured]
    }
}

async fn null_rps(workers: usize) -> f64 {
    let deadline = Instant::now() + WINDOW;
    let mut tasks = JoinSet::new();
    for _ in 0..workers {
        tasks.spawn(async move {
            let mut count = 0_u64;
            let mut value = 0_u64;
            while Instant::now() < deadline {
                value = black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
                count += 1;
                if count % 256 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            black_box(value);
            count
        });
    }
    let mut count = 0_u64;
    while let Some(result) = tasks.join_next().await {
        count += result.expect("calibration task does not panic");
    }
    count as f64 / WINDOW.as_secs_f64()
}

async fn loopback_rps(workers: usize, body_bytes: usize, tls: bool) -> Result<f64, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "cannot start loopback calibration target")?;
    let address = listener
        .local_addr()
        .map_err(|_| "cannot inspect loopback calibration target")?;
    let payload = Arc::<[u8]>::from(vec![b'x'; body_bytes.clamp(1, 64 * 1024)]);
    let deadline = Instant::now() + WINDOW;
    let mut tasks = JoinSet::new();
    let server = if tls {
        let (acceptor, connector) = tls_pair()?;
        let server = tokio::spawn(echo_tls(listener, acceptor));
        for _ in 0..workers {
            let payload = Arc::clone(&payload);
            let connector = connector.clone();
            tasks
                .spawn(async move { echo_tls_client(address, connector, payload, deadline).await });
        }
        server
    } else {
        let server = tokio::spawn(echo_plain(listener));
        for _ in 0..workers {
            let payload = Arc::clone(&payload);
            tasks.spawn(async move { echo_plain_client(address, payload, deadline).await });
        }
        server
    };

    let mut count = 0_u64;
    while let Some(result) = tasks.join_next().await {
        count += result.map_err(|_| "loopback calibration task failed")??;
    }
    server.abort();
    Ok(count as f64 / WINDOW.as_secs_f64())
}

fn tls_pair() -> Result<(TlsAcceptor, TlsConnector), String> {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .map_err(|_| "cannot create loopback TLS certificate")?;
    let mut roots = RootCertStore::empty();
    roots
        .add(certificate.cert.der().clone())
        .map_err(|_| "cannot trust loopback TLS certificate")?;
    let client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| "cannot configure loopback TLS client")?
            .with_root_certificates(roots)
            .with_no_client_auth();
    let server =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| "cannot configure loopback TLS server")?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .map_err(|_| "cannot configure loopback TLS certificate")?;
    Ok((
        TlsAcceptor::from(Arc::new(server)),
        TlsConnector::from(Arc::new(client)),
    ))
}

async fn echo_plain(listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            echo_stream(&mut socket).await;
        });
    }
}

async fn echo_tls(listener: TcpListener, acceptor: TlsAcceptor) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Ok(mut stream) = acceptor.accept(socket).await {
                echo_stream(&mut stream).await;
            }
        });
    }
}

async fn echo_stream(stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) {
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 || stream.write_all(&buffer[..read]).await.is_err() {
            return;
        }
    }
}

async fn echo_plain_client(
    address: std::net::SocketAddr,
    payload: Arc<[u8]>,
    deadline: Instant,
) -> Result<u64, String> {
    let mut socket = TcpStream::connect(address)
        .await
        .map_err(|_| "loopback calibration connection failed")?;
    echo_client(&mut socket, payload, deadline).await
}

async fn echo_tls_client(
    address: std::net::SocketAddr,
    connector: TlsConnector,
    payload: Arc<[u8]>,
    deadline: Instant,
) -> Result<u64, String> {
    let socket = TcpStream::connect(address)
        .await
        .map_err(|_| "loopback TLS connection failed")?;
    let name = ServerName::try_from("localhost")
        .expect("static TLS server name")
        .to_owned();
    let mut stream = connector
        .connect(name, socket)
        .await
        .map_err(|_| "loopback TLS handshake failed")?;
    echo_client(&mut stream, payload, deadline).await
}

async fn echo_client(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    payload: Arc<[u8]>,
    deadline: Instant,
) -> Result<u64, String> {
    let mut response = vec![0_u8; payload.len()];
    let mut count = 0_u64;
    while Instant::now() < deadline {
        stream
            .write_all(&payload)
            .await
            .map_err(|_| "loopback calibration write failed")?;
        stream
            .read_exact(&mut response)
            .await
            .map_err(|_| "loopback calibration read failed")?;
        count += 1;
    }
    Ok(count)
}

fn profile_id(hardware: &Hardware, shape: &Shape, ceilings: &[Ceiling]) -> String {
    let bytes =
        serde_json::to_vec(&(hardware, shape, ceilings)).expect("profile identity serializes");
    format!("sha256:{:x}", Sha256::digest(bytes))
}
