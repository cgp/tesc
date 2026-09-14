//! Compile the bundle once, before a single request is sent.
//!
//! Unsupported semantics must never be silently ignored: a plan that names something
//! this engine cannot do is refused, with the path to the field that says so. The
//! alternative is a run that completes and measures something other than what was
//! asked for, which is the one failure this tool cannot afford.
//!
//! Calls are resolved here rather than executed from: `calls.rs` turns every `call`
//! a step names into the request it will send, so an unresolvable reference in the
//! sixth chain is a load-time error rather than a surprise four minutes in.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use metrix_plan::{Call, CallFile, LoadMode, LoadModel, Mix, SessionPolicy, Target, Targets};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::calls;
use crate::schedule::Schedule;

pub struct Plan {
    root: std::path::PathBuf,
    pub(crate) name: String,
    pub(crate) hash: String,
    pub(crate) chain: String,
    pub(crate) step: String,
    pub(crate) call: String,
    pub(crate) target: Target,
    /// The chain the executor runs, steps in order. One chain, until B3.3 mixes
    /// several.
    pub(crate) chain_steps: Arc<crate::chain::Compiled>,
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

impl Plan {
    pub fn bundle_root(&self) -> &Path {
        &self.root
    }

    /// The chain this plan runs, and the steps within it.
    /// The longest any one request in the chain may take. What the connection pool
    /// is prepared with, because a pool that gave up sooner than the request it is
    /// carrying would report the generator's impatience as the service's failure.
    pub(crate) fn request_timeout(&self) -> Duration {
        self.chain_steps
            .steps
            .iter()
            .map(|step| step.request.timeout)
            .max()
            .unwrap_or_default()
    }

    pub(crate) fn chain_name(&self) -> &'static str {
        self.chain_steps.name
    }

    pub(crate) fn step_name(&self, index: usize) -> &'static str {
        self.chain_steps.steps[index].id
    }

    /// The first request this plan will send, for tests that need to see what
    /// compiled. An opaque handle rather than the field, so nothing outside the
    /// engine can assemble a request of its own from the parts.
    #[doc(hidden)]
    pub fn request_for_test(&self) -> RequestView<'_> {
        let request = &self.chain_steps.steps[0].request;
        let prepared = request.prepared().expect("a fixed call in a test");
        RequestView {
            uri: &prepared.uri,
            method: &request.method,
        }
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
        let mut defined: BTreeMap<String, Call> = BTreeMap::new();
        for file in &mix.calls {
            for (name, call) in read::<CallFile>(&root, file, &mut documents)? {
                require(
                    !name.is_empty(),
                    "mix.json/calls: a call name must not be empty",
                )?;
                require(
                    defined.insert(name.clone(), call).is_none(),
                    &format!(
                        "mix.json/calls: {name:?} is defined in more than one file; a step \
                         naming it could not say which"
                    ),
                )?;
            }
        }

        // Every chain, every step, every reference -- not only the one that will be
        // sent. The layers that use the rest arrive in B3.2 and B3.3; the resolution
        // they will use is a property of the document, and is checked as one.
        let resolved = calls::resolve(&mix, &defined, &target, &authority)?;

        require(
            resolved.chains.len() == 1,
            "mix.json/chains: mixing several chains is not implemented yet (B3.3)",
        )?;
        let chain = &resolved.chains[0];
        require(
            chain.percent == 100.0,
            "mix.json/chains/0/percent: a single chain takes all of the traffic",
        )?;
        require(
            chain.session == SessionPolicy::Fresh && chain.pool_size.is_none(),
            "mix.json/chains/0/session: stateless fresh sessions only (B3.9)",
        )?;
        for (position, written) in mix.chains[0].steps.iter().enumerate() {
            require(
                written.overrides.is_none()
                    && written.delay_ms.is_none()
                    && written.on_failure.is_none()
                    && written.repeat_until.is_none(),
                &format!(
                    "mix.json/chains/0/steps/{position}: overrides, delays and                      failure/repeat policies are not implemented (B3.4)"
                ),
            )?;
        }
        let step = &chain.steps[0];
        // Leaked deliberately: these name the chain and its steps for the life of
        // the process, and every accumulator map is keyed by them.
        let chain_steps = Arc::new(crate::chain::Compiled {
            name: String::leak(chain.name.clone()),
            steps: chain
                .steps
                .iter()
                .map(|step| crate::chain::Step {
                    id: String::leak(step.id.clone()),
                    request: Arc::clone(&resolved.requests[&step.call]),
                })
                .collect(),
        });
        let timeout = resolved.longest_timeout();
        require(
            span.checked_add(timeout)
                .and_then(|d| std::time::Instant::now().checked_add(d))
                .is_some(),
            "mix.json/phases: timeline duration including drain is not representable",
        )?;
        let calibration_shape = crate::calibration::Shape {
            request_body_bytes: calibration_body_bytes(&chain_steps),
            tls: target.tls.enabled,
            chain_depth: chain_steps.steps.len() as u32,
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
            chain_steps,
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

/// The body size calibration measures against: the largest a single request in the
/// chain can be. Calibration asks what this machine can push, and the widest request
/// is what decides that.
fn calibration_body_bytes(chain: &crate::chain::Compiled) -> usize {
    chain
        .steps
        .iter()
        .map(|step| {
            step.request
                .prepared()
                .map_or(0, |prepared| prepared.body.len())
        })
        .max()
        .unwrap_or(0)
}

/// What a test may see of a compiled request.
#[doc(hidden)]
pub struct RequestView<'a> {
    pub uri: &'a hyper::Uri,
    pub method: &'a hyper::Method,
}

pub(crate) fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
