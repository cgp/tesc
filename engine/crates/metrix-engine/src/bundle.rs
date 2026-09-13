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

use crate::schedule::Schedule;

pub struct Plan {
    pub(crate) target: Target,
    pub(crate) request: Arc<RequestTemplate>,
    pub(crate) rate: f64,
    pub(crate) duration: Duration,
    pub(crate) concurrency: usize,
    pub(crate) connections: usize,
    pub worker_threads: usize,
}

pub(crate) struct RequestTemplate {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub timeout: Duration,
}

impl Plan {
    pub fn load(root: &Path) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|_| "--plan: cannot open bundle directory")?;
        let mix: Mix = read(&root, Path::new("mix.json"))?;
        let targets: Targets = read(&root, Path::new("targets.json"))?;
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
            mix.phases.baseline.is_zero()
                && mix.phases.settle.is_zero()
                && mix.load.warmup.is_none_or(|d| d.is_zero()),
            "mix.json/phases: set baseline, warmup and settle to zero until B2.1",
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
            for (name, call) in read::<CallFile>(&root, file)? {
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
        Ok(Self {
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
            concurrency,
            connections,
            worker_threads,
        })
    }
}

fn read<T: DeserializeOwned>(root: &Path, relative: &Path) -> Result<T, String> {
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
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "bundle document: invalid JSON or document shape at line {}, column {}",
            error.line(),
            error.column()
        )
    })
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
