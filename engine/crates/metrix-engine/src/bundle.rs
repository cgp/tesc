//! Compile the B1.2 subset once. Unsupported semantics must never be silently ignored.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use hyper::{
    HeaderMap, Method, Uri,
    header::{HeaderName, HeaderValue},
};
use metrix_plan::{Body, CallFile, LoadMode, LoadModel, Mix, SessionPolicy, Target, Targets};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::schedule::Schedule;

pub struct Plan {
    root: std::path::PathBuf,
    pub(crate) name: String,
    pub(crate) hash: String,
    pub(crate) chain: String,
    pub(crate) step: String,
    pub(crate) call: String,
    pub(crate) target: Target,
    pub(crate) request: Arc<RequestTemplate>,
    pub(crate) rate: f64,
    pub(crate) duration: Duration,
    pub(crate) baseline: Duration,
    pub(crate) warmup: Duration,
    pub(crate) settle: Duration,
    pub(crate) concurrency: usize,
    pub(crate) connections: usize,
    pub worker_threads: usize,
    pub detector_config: crate::DetectorConfig,
    pub(crate) calibration_shape: crate::calibration::Shape,
    pub(crate) machine_profile: Option<crate::MachineProfile>,
    pub(crate) headroom_ratio: Option<f64>,
    pub(crate) allow_generator_limited: bool,
}

pub(crate) struct RequestTemplate {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub timeout: Duration,
}

impl Plan {
    pub fn bundle_root(&self) -> &Path {
        &self.root
    }

    pub fn load(root: &Path) -> Result<Self, String> {
        Self::load_inner(root, true)
    }

    /// Calibration replaces a stale local profile, so it deliberately ignores one while
    /// compiling the plan shape.
    pub fn load_for_calibration(root: &Path) -> Result<Self, String> {
        Self::load_inner(root, false)
    }

    fn load_inner(root: &Path, load_machine_profile: bool) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|_| "--plan: cannot open bundle directory")?;
        let mut documents = BTreeMap::new();
        let mix: Mix = read(&root, Path::new("mix.json"), &mut documents)?;
        let targets: Targets = read(&root, Path::new("targets.json"), &mut documents)?;
        require(
            mix.version == 1,
            "mix.json/version: only version 1 is supported",
        )?;
        require(
            mix.load.mode == LoadMode::Fixed && mix.load.model == LoadModel::Open,
            "mix.json/load: B1.2 requires fixed, open-model load",
        )?;
        require(
            mix.load.stages.is_empty() && mix.load.breakpoint.is_none(),
            "mix.json/load: stages and breakpoint are not implemented",
        )?;
        require(
            mix.auth.is_none() && mix.datasets.is_empty() && mix.generators.is_empty(),
            "mix.json: auth, datasets and generators are not implemented",
        )?;
        require(
            mix.slo.is_empty() && mix.observe.is_none(),
            "mix.json: SLOs and observation are not available in B1.2",
        )?;
        require(
            mix.defaults.follow_redirects != Some(true),
            "mix.json/defaults/follow_redirects: redirects are not implemented",
        )?;
        require(
            mix.engine.pin_cores != Some(true),
            "mix.json/engine/pin_cores: core pinning is not implemented",
        )?;
        require(
            targets.list.len() == 1 && targets.gap.is_none_or(|d| d.is_zero()),
            "targets.json: B1.2 requires exactly one target and no inter-target gap",
        )?;
        let target = targets.list.into_iter().next().expect("checked length");
        require(
            !target.id.is_empty(),
            "targets.json/list/0/id: must not be empty",
        )?;
        require(
            target.host_header.is_none()
                && target.tls.sni.is_none()
                && !target.tls.insecure_skip_verify,
            "targets.json/list/0: Host/SNI overrides and insecure TLS are not implemented (B4.2)",
        )?;
        let authority: hyper::http::uri::Authority = target
            .address
            .parse()
            .map_err(|_| "targets.json/list/0/address: expected host:port or [IPv6]:port")?;
        require(
            authority.port_u16().is_some_and(|p| p > 0)
                && !authority.host().is_empty()
                && !target.address.contains('@'),
            "targets.json/list/0/address: expected host:port or [IPv6]:port",
        )?;
        let rate = mix
            .load
            .rate
            .ok_or("mix.json/load/rate: required for fixed load")?;
        let duration = mix.load.duration.as_duration();
        Schedule::validate(rate, duration)?;
        let tolerance = mix.engine.rate_tolerance_pct.unwrap_or(2);
        let explicit_drift = mix.engine.send_drift_threshold_ms;
        require(
            tolerance < 100 && explicit_drift.is_none_or(|ms| (1..=3_600_000).contains(&ms)),
            "mix.json/engine: detector tolerance must be 0..99 and drift threshold 1..3600000ms",
        )?;
        let detector_config = crate::DetectorConfig {
            rate_tolerance_pct: tolerance,
            drift_threshold: explicit_drift
                .map(Duration::from_millis)
                .unwrap_or_else(|| {
                    Duration::from_secs_f64((1.0 / rate).min(3600.0)).max(Duration::from_millis(5))
                }),
        };
        let baseline = mix.phases.baseline.as_duration();
        let warmup = mix.load.warmup.map_or(Duration::ZERO, |d| d.as_duration());
        let settle = mix.phases.settle.as_duration();
        if !warmup.is_zero() {
            Schedule::validate(rate, warmup)?;
        }
        let span = baseline
            .checked_add(warmup)
            .and_then(|d| d.checked_add(duration))
            .and_then(|d| d.checked_add(settle))
            .ok_or("mix.json/phases: timeline duration is not representable")?;
        require(
            std::time::Instant::now().checked_add(span).is_some(),
            "mix.json/phases: timeline duration is not representable",
        )?;
        let concurrency = mix.load.max_concurrency.unwrap_or(200) as usize;
        let connections = mix.engine.connections_per_host.unwrap_or(256) as usize;
        require(
            concurrency > 0 && connections > 0,
            "mix.json: max_concurrency and connections_per_host must be positive",
        )?;
        let worker_threads = mix
            .engine
            .worker_threads
            .unwrap_or_else(|| num_cpus::get_physical().saturating_sub(1).max(1));
        require(
            worker_threads > 0,
            "mix.json/engine/worker_threads: must be positive",
        )?;
        require(
            mix.chains.len() == 1,
            "mix.json/chains: B1.2 requires one single-call chain",
        )?;
        let chain = &mix.chains[0];
        require(
            chain.percent == 100.0 && chain.steps.len() == 1 && !chain.name.is_empty(),
            "mix.json/chains/0: requires a name, 100 percent and exactly one step",
        )?;
        require(
            chain.session == SessionPolicy::Fresh && chain.pool_size.is_none(),
            "mix.json/chains/0/session: B1.2 supports stateless fresh sessions only",
        )?;
        let step = &chain.steps[0];
        require(
            !step.id.is_empty()
                && step.overrides.is_none()
                && step.delay_ms.is_none()
                && step.on_failure.is_none()
                && step.repeat_until.is_none(),
            "mix.json/chains/0/steps/0: requires an id; overrides, delays and failure/repeat policies are not implemented",
        )?;
        let mut calls = BTreeMap::new();
        for file in &mix.calls {
            for (name, call) in read::<CallFile>(&root, file, &mut documents)? {
                require(
                    !name.is_empty() && calls.insert(name, call).is_none(),
                    "mix.json/calls: empty or duplicate call name",
                )?;
            }
        }
        let call = calls
            .get(&step.call)
            .ok_or("mix.json/chains/0/steps/0/call: unresolved call reference")?;
        require(
            call.assertions.is_empty() && call.extract.is_empty(),
            "call: assertions and extraction are not implemented",
        )?;
        let body = match &call.body {
            None => Bytes::new(),
            Some(Body::Inline(text)) => {
                static_text(text)?;
                Bytes::copy_from_slice(text.as_bytes())
            }
            Some(Body::Generated { .. }) => {
                return Err("call/body: generators are not implemented".into());
            }
        };
        static_text(&call.path)?;
        require(
            call.path.starts_with('/') && !call.path.starts_with("//") && !call.path.contains('#'),
            "call/path: expected an origin-relative path without a fragment",
        )?;
        let mut path = call.path.clone();
        for (name, value) in &call.query {
            static_text(name)?;
            static_text(value)?;
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str(&encode_query(name));
            path.push('=');
            path.push_str(&encode_query(value));
        }
        let scheme = if target.tls.enabled { "https" } else { "http" };
        let uri = format!("{scheme}://{authority}{path}")
            .parse::<Uri>()
            .map_err(|_| "call/path: invalid HTTP URI")?;
        let mut headers = HeaderMap::new();
        for (name, value) in mix.defaults.headers.iter().chain(call.headers.iter()) {
            static_text(value)?;
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "call/headers: invalid header name")?;
            require(
                !matches!(
                    name.as_str(),
                    "host"
                        | "connection"
                        | "proxy-connection"
                        | "keep-alive"
                        | "upgrade"
                        | "transfer-encoding"
                        | "content-length"
                        | "te"
                        | "trailer"
                ),
                "call/headers: transport-managed header is not allowed",
            )?;
            headers.insert(
                name,
                HeaderValue::from_str(value).map_err(|_| "call/headers: invalid header value")?,
            );
        }
        headers.insert(
            hyper::header::HOST,
            HeaderValue::from_str(authority.as_str())
                .map_err(|_| "target: invalid HTTP authority")?,
        );
        let timeout =
            Duration::from_millis(call.timeout_ms.or(mix.defaults.timeout_ms).unwrap_or(5000));
        require(
            !timeout.is_zero() && std::time::Instant::now().checked_add(timeout).is_some(),
            "call/timeout_ms: must be positive and representable by the monotonic clock",
        )?;
        require(
            span.checked_add(timeout)
                .and_then(|d| std::time::Instant::now().checked_add(d))
                .is_some(),
            "mix.json/phases: timeline duration including drain is not representable",
        )?;
        let calibration_shape = crate::calibration::Shape {
            request_body_bytes: body.len(),
            tls: target.tls.enabled,
            chain_depth: 1,
            generation: "static".into(),
        };
        let machine_profile = if load_machine_profile {
            crate::MachineProfile::load(&root, &calibration_shape, worker_threads)?
        } else {
            None
        };
        let headroom_ratio = machine_profile
            .as_ref()
            .map(|profile| rate / profile.ceiling(worker_threads));
        let allow_generator_limited = mix.engine.allow_generator_limited.unwrap_or(false);
        require(
            headroom_ratio.is_none_or(|ratio| ratio <= 0.9 || allow_generator_limited),
            "mix.json/load/rate: exceeds 90% of the calibrated generator ceiling; set engine/allow_generator_limited to true to run with an invalid annotation",
        )?;
        Ok(Self {
            root,
            name: mix.name.clone(),
            hash: bundle_hash(&documents),
            chain: chain.name.clone(),
            step: step.id.clone(),
            call: step.call.clone(),
            target,
            request: Arc::new(RequestTemplate {
                method: call.method.to_string().parse().expect("plan method enum"),
                uri,
                headers,
                body,
                timeout,
            }),
            rate,
            duration,
            baseline,
            warmup,
            settle,
            concurrency,
            connections,
            worker_threads,
            detector_config,
            calibration_shape,
            machine_profile,
            headroom_ratio,
            allow_generator_limited,
        })
    }
}

fn read<T: DeserializeOwned>(
    root: &Path,
    relative: &Path,
    documents: &mut BTreeMap<String, Vec<u8>>,
) -> Result<T, String> {
    require(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "bundle: file references must be relative paths without traversal",
    )?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| "bundle: referenced file is missing or unreadable")?;
    require(
        path.starts_with(root),
        "bundle: referenced file escapes the bundle root",
    )?;
    let bytes = fs::read(path).map_err(|_| "bundle: cannot read referenced file")?;
    // Serde errors can quote input values. Retain location, never potentially secret input.
    let document = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "bundle document: invalid JSON or document shape at line {}, column {}",
            error.line(),
            error.column()
        )
    })?;
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    documents.insert(name, bytes);
    Ok(document)
}

fn bundle_hash(documents: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hash = Sha256::new();
    for (path, bytes) in documents {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("sha256:{:x}", hash.finalize())
}

fn static_text(value: &str) -> Result<(), String> {
    require(
        !value.contains("{{") && !value.contains("}}"),
        "call: templates are not implemented",
    )
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn encode_query(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}
