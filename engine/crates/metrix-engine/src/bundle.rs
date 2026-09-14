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

use metrix_plan::{
    Call, CallFile, LoadMode, LoadModel, Mix, PERCENT_EPSILON, PERCENT_TOTAL, SessionPolicy,
    Target, Targets,
};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::calls;
use crate::schedule::Schedule;

pub struct Plan {
    pub(crate) targets: Targets,
    pub(crate) target_index: usize,
    targets_override: Option<std::path::PathBuf>,
    root: std::path::PathBuf,
    pub(crate) name: String,
    pub(crate) hash: String,
    pub(crate) target: Target,
    /// Every chain the mixture holds, each with its steps in order.
    pub(crate) chains: Vec<Arc<crate::chain::Compiled>>,
    /// Every dataset the mix declares, read once and shared by every iteration.
    pub(crate) datasets: Arc<crate::dataset::Datasets>,
    /// Every generator the mix declares, compiled once and shared the same way.
    pub(crate) generators: Arc<crate::generate::Generators>,
    /// The error-sample budget and the redaction rules, shared by every send.
    pub(crate) samples: Arc<crate::samples::Samples>,
    /// What each chain's virtual users carry between requests, in chain order.
    pub(crate) sessions: crate::session::PerChain,
    /// Each chain's declared policy, kept so `--chain` can rebuild one set.
    chain_sessions: Vec<(SessionPolicy, Option<u32>)>,
    /// Set when the run was narrowed to one chain, so the output can say so.
    only: Option<String>,
    /// The credential every request carries, and what keeps it fresh. `None` when the
    /// plan declares no auth, which is not the same as `mode: none` costing nothing.
    pub(crate) auth: Option<Arc<crate::auth::Auth>>,
    /// The run seed. Set from `--seed` rather than from the bundle: it names one run
    /// of the plan, not the plan, and it is what a replay is asked for.
    pub(crate) seed: u64,
    /// Each chain's share of the total rate, in the same order.
    pub(crate) weights: Vec<f64>,
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
    /// The longest any one request in the mixture may take. What the connection
    /// pool is prepared with, because a pool that gave up sooner than the request it
    /// is carrying would report the generator's impatience as the service's failure.
    pub(crate) fn request_timeout(&self) -> Duration {
        self.chains
            .iter()
            .flat_map(|chain| chain.steps.iter())
            .map(|step| step.request.timeout)
            .max()
            .unwrap_or_default()
    }

    /// The first request this plan will send, for tests that need to see what
    /// compiled. An opaque handle rather than the field, so nothing outside the
    /// engine can assemble a request of its own from the parts.
    #[doc(hidden)]
    pub fn request_for_test(&self) -> RequestView<'_> {
        let request = &self.chains[0].steps[0].request;
        let prepared = request.prepared().expect("a fixed call in a test");
        RequestView {
            uri: &prepared.uri,
            method: &request.method,
        }
    }

    pub fn load(root: &Path) -> Result<Self, String> {
        Self::load_inner(root, true, 0, None)
    }

    /// Run one chain of the mixture on its own.
    ///
    /// For working on a plan rather than measuring with one: a chain at 3% of the
    /// rate sends a request every few seconds, and finding out whether its extraction
    /// works should not take four minutes. The chain runs at the whole rate, and the
    /// run says which chain it was — the numbers are about that chain and not about
    /// the mixture, and a report that did not say so would be a mixture nobody wrote.
    pub fn only_chain(&mut self, name: &str) -> Result<(), String> {
        let index = self
            .chains
            .iter()
            .position(|chain| chain.name == name)
            .ok_or_else(|| {
                format!(
                    "--chain: no chain named {name:?}; the mixture has {}",
                    self.chains
                        .iter()
                        .map(|chain| chain.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        self.chains = vec![Arc::clone(&self.chains[index])];
        self.weights = vec![PERCENT_TOTAL];
        self.sessions = Arc::new(vec![crate::session::Sessions::new(
            self.chain_sessions[index].0,
            self.concurrency,
            self.chain_sessions[index].1,
        )]);
        self.only = Some(name.to_owned());
        Ok(())
    }

    /// The chain this run was narrowed to, if it was.
    pub fn narrowed_to(&self) -> Option<&str> {
        self.only.as_deref()
    }

    /// The seed every generated value in the run comes from.
    ///
    /// Not part of the bundle: the bundle is the plan and the seed names one run of
    /// it. Recorded in the run's identity so a replay can be asked for by it.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
    }

    /// Calibration replaces a stale local profile, so it deliberately ignores one while
    /// compiling the plan shape.
    pub fn load_for_calibration(root: &Path) -> Result<Self, String> {
        Self::load_inner(root, false, 0, None)
    }

    pub fn load_with_targets(root: &Path, targets: &Path) -> Result<Self, String> {
        Self::load_inner(root, true, 0, Some(targets))
    }
    pub(crate) fn for_target(&self, index: usize) -> Result<Self, String> {
        let mut plan = Self::load_inner(&self.root, true, index, self.targets_override.as_deref())?;
        plan.set_seed(self.seed);
        if let Some(chain) = &self.only {
            plan.only_chain(chain)?;
        }
        Ok(plan)
    }
    pub(crate) fn target_order(&self) -> Vec<usize> {
        let mut order: Vec<_> = (0..self.targets.list.len()).collect();
        if self.targets.order == metrix_plan::targets::TargetOrder::Shuffle {
            let mut rng = crate::random::Rng::seeded(self.seed, 0);
            for i in (1..order.len()).rev() {
                let j = rng.next_u64() as usize % (i + 1);
                order.swap(i, j);
            }
        }
        order
    }
    fn load_inner(
        root: &Path,
        load_machine_profile: bool,
        target_index: usize,
        targets_override: Option<&Path>,
    ) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|_| "--plan: cannot open bundle directory")?;
        let mut documents = BTreeMap::new();
        let mix: Mix = read(&root, Path::new("mix.json"), &mut documents)?;
        let targets: Targets = if let Some(path) = targets_override {
            let bytes = fs::read(path).map_err(|_| "--targets: cannot read document")?;
            let targets = serde_json::from_slice(&bytes)
                .map_err(|_| "--targets: invalid targets document")?;
            documents.insert("targets.json".into(), bytes);
            targets
        } else {
            read(&root, Path::new("targets.json"), &mut documents)?
        };
        require(
            mix.version == 1,
            "mix.json#/version: only version 1 is supported",
        )?;
        require(
            mix.load.mode == LoadMode::Fixed && mix.load.model == LoadModel::Open,
            "mix.json#/load: B1.2 requires fixed, open-model load",
        )?;
        require(
            mix.load.stages.is_empty() && mix.load.breakpoint.is_none(),
            "mix.json#/load: stages and breakpoint are not implemented",
        )?;
        require(
            mix.slo.is_empty() && mix.observe.is_none(),
            "mix.json#/slo: SLOs and observation are not available in B1.2",
        )?;
        require(
            mix.defaults.follow_redirects != Some(true),
            "mix.json#/defaults/follow_redirects: redirects are not implemented",
        )?;
        require(
            mix.engine.pin_cores != Some(true),
            "mix.json#/engine/pin_cores: core pinning is not implemented",
        )?;
        require(
            !targets.list.is_empty(),
            "targets.json#/list: must not be empty",
        )?;
        let mut ids = std::collections::BTreeSet::new();
        for (i, target) in targets.list.iter().enumerate() {
            require(
                !target.id.is_empty() && ids.insert(&target.id),
                &format!("targets.json#/list/{i}/id: must be nonempty and unique"),
            )?;
            let authority: hyper::http::uri::Authority = target.address.parse().map_err(|_| {
                format!("targets.json#/list/{i}/address: expected host:port or [IPv6]:port")
            })?;
            require(
                authority.port_u16().is_some_and(|p| p > 0)
                    && !authority.host().is_empty()
                    && !target.address.contains('@'),
                &format!("targets.json#/list/{i}/address: expected host:port or [IPv6]:port"),
            )?;
            require(
                target.host_header.is_none()
                    && target.tls.sni.is_none()
                    && !target.tls.insecure_skip_verify,
                &format!(
                    "targets.json#/list/{i}: Host/SNI overrides and insecure TLS are not implemented (B4.2)"
                ),
            )?;
        }
        let target = targets.list[target_index].clone();
        let authority = target.address.parse().expect("validated authority");
        let rate = mix
            .load
            .rate
            .ok_or("mix.json#/load/rate: required for fixed load")?;
        let duration = mix.load.duration.as_duration();
        Schedule::validate(rate, duration)?;
        let tolerance = mix.engine.rate_tolerance_pct.unwrap_or(2);
        let explicit_drift = mix.engine.send_drift_threshold_ms;
        require(
            tolerance < 100 && explicit_drift.is_none_or(|ms| (1..=3_600_000).contains(&ms)),
            "mix.json#/engine: detector tolerance must be 0..99 and drift threshold 1..3600000ms",
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
            .ok_or("mix.json#/phases: timeline duration is not representable")?;
        require(
            std::time::Instant::now().checked_add(span).is_some(),
            "mix.json#/phases: timeline duration is not representable",
        )?;
        let concurrency = mix.load.max_concurrency.unwrap_or(200) as usize;
        let connections = mix.engine.connections_per_host.unwrap_or(256) as usize;
        require(
            concurrency > 0 && connections > 0,
            "mix.json#/load/max_concurrency: max_concurrency and connections_per_host must be positive",
        )?;
        let worker_threads = mix
            .engine
            .worker_threads
            .unwrap_or_else(|| num_cpus::get_physical().saturating_sub(1).max(1));
        require(
            worker_threads > 0,
            "mix.json#/engine/worker_threads: must be positive",
        )?;
        // Each call remembers which file it came from, so an error about it names the
        // file to open rather than only the call. A bundle can hold a dozen call
        // files, and `call "create-order"/path` says nothing about where that is.
        let mut defined: BTreeMap<String, (String, Call)> = BTreeMap::new();
        for file in &mix.calls {
            let where_from = file.to_string_lossy().replace('\\', "/");
            for (name, call) in read::<CallFile>(&root, file, &mut documents)? {
                require(
                    !name.is_empty(),
                    "mix.json#/calls: a call name must not be empty",
                )?;
                require(
                    defined
                        .insert(name.clone(), (where_from.clone(), call))
                        .is_none(),
                    &format!(
                        "mix.json#/calls: {name:?} is defined in more than one file; a step \
                         naming it could not say which"
                    ),
                )?;
            }
        }

        // Every chain, every step, every reference -- not only the one that will be
        // sent. The layers that use the rest arrive in B3.2 and B3.3; the resolution
        // they will use is a property of the document, and is checked as one.
        let datasets = Arc::new(crate::dataset::Datasets::load(&root, &mix)?);
        let generators = Arc::new(crate::generate::Generators::load(&root, &mix)?);
        let resolved = calls::resolve(&mix, &defined, &datasets, &generators, &target, &authority)?;
        // Compiled against the same context a call is, because a login request is a
        // call: same templates, same extractors, same refusals.
        let auth = mix
            .auth
            .as_ref()
            .map(|declared| {
                crate::auth::Auth::compile(
                    declared,
                    concurrency,
                    &calls::Context {
                        defaults: &mix.defaults,
                        body_max: mix.capture.body_max_kb as usize * 1024,
                        datasets: &datasets,
                        generators: &generators,
                        target: &target,
                        authority: &authority,
                    },
                )
            })
            .transpose()?
            .flatten()
            .map(Arc::new);
        unique_rows_suffice(&datasets, &resolved, rate, warmup + duration)?;

        let timeout = resolved.longest_timeout();
        // Percentages are a claim about what the service was asked for, so they
        // have to add up before anything is sent. Named as a shortfall or an excess
        // and never renormalized: adjusting five chains to accommodate a typo in the
        // sixth would measure a mixture nobody wrote.
        let total: f64 = resolved.chains.iter().map(|chain| chain.percent).sum();
        require(
            (total - PERCENT_TOTAL).abs() <= PERCENT_EPSILON,
            &format!(
                "mix.json#/chains: the percentages total {total:.4}, which is {:.4} {} 100",
                (PERCENT_TOTAL - total).abs(),
                if total < PERCENT_TOTAL {
                    "short of"
                } else {
                    "over"
                },
            ),
        )?;

        for (index, chain) in resolved.chains.iter().enumerate() {
            require(
                chain.percent > 0.0,
                &format!(
                    "mix.json#/chains/{index}/percent: a chain at {} never runs; remove it, or give it a share of the traffic",
                    chain.percent
                ),
            )?;
            require(
                chain.session != SessionPolicy::Pool || chain.pool_size.is_some_and(|n| n > 0),
                &format!(
                    "mix.json#/chains/{index}/pool_size: a pooled chain needs a population                      size; without one the pool is one session and the policy is `reuse`"
                ),
            )?;
            require(
                chain.session == SessionPolicy::Pool || chain.pool_size.is_none(),
                &format!(
                    "mix.json#/chains/{index}/pool_size: only a pooled chain has a population"
                ),
            )?;
            for (position, written) in mix.chains[index].steps.iter().enumerate() {
                require(
                    written.overrides.is_none() && written.delay_ms.is_none(),
                    &format!(
                        "mix.json#/chains/{index}/steps/{position}: step overrides and think time are not implemented"
                    ),
                )?;
            }
        }

        // Leaked deliberately: these name the chains and their steps for the life of
        // the process, and every accumulator map is keyed by them.
        let weights: Vec<f64> = resolved.chains.iter().map(|chain| chain.percent).collect();
        // One set per chain, because the policy is the chain's: a plan whose checkout
        // is a first-time user and whose search is a returning one is the ordinary
        // case, not an exception.
        let samples = Arc::new(crate::samples::Samples::new(
            mix.capture.error_samples,
            &mix.capture.redact,
            mix.capture.body_max_kb as usize * 1024,
        ));
        let chain_sessions: Vec<(SessionPolicy, Option<u32>)> = resolved
            .chains
            .iter()
            .map(|chain| (chain.session, chain.pool_size))
            .collect();
        let sessions: crate::session::PerChain = Arc::new(
            resolved
                .chains
                .iter()
                .map(|chain| {
                    crate::session::Sessions::new(chain.session, concurrency, chain.pool_size)
                })
                .collect(),
        );
        let chains: Vec<_> = resolved
            .chains
            .into_iter()
            .map(|chain| {
                Arc::new(crate::chain::Compiled {
                    name: String::leak(chain.name),
                    steps: chain
                        .steps
                        .into_iter()
                        .map(|step| crate::chain::Step {
                            request: step.request,
                            id: String::leak(step.id),
                            call: String::leak(step.call),
                            on_failure: step.on_failure,
                            repeat_until: step.repeat_until,
                        })
                        .collect(),
                })
            })
            .collect();

        require(
            span.checked_add(timeout)
                .and_then(|d| std::time::Instant::now().checked_add(d))
                .is_some(),
            "mix.json#/phases: timeline duration including drain is not representable",
        )?;
        let calibration_shape = crate::calibration::Shape {
            request_body_bytes: calibration_body_bytes(&chains),
            tls: target.tls.enabled,
            chain_depth: chains
                .iter()
                .map(|chain| chain.steps.len())
                .max()
                .unwrap_or(0) as u32,
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
            "mix.json#/load/rate: exceeds 90% of the calibrated generator ceiling; set engine/allow_generator_limited to true to run with an invalid annotation",
        )?;
        Ok(Self {
            targets,
            target_index,
            targets_override: targets_override.map(Path::to_path_buf),
            root,
            name: mix.name.clone(),
            hash: bundle_hash(&documents),
            target,
            chains,
            datasets,
            generators,
            samples,
            sessions,
            chain_sessions,
            only: None,
            auth,
            seed: 0,
            weights,
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

/// A `unique_per_iteration` dataset has to hold a row for every iteration that will
/// read it.
///
/// The mode exists for POSTs that must not collide (§7.2), so wrapping quietly at the
/// end of the file would take away the one thing it promises. Checked against the
/// arithmetic of the run rather than discovered partway through it: a file 200 rows
/// short is 200 colliding requests in results nobody will re-read.
fn unique_rows_suffice(
    datasets: &crate::dataset::Datasets,
    resolved: &calls::Resolved,
    rate: f64,
    sending: Duration,
) -> Result<(), String> {
    for (index, dataset) in datasets.iter().enumerate() {
        if dataset.mode() != metrix_plan::DatasetMode::UniquePerIteration {
            continue;
        }
        // Only the chains that read it, at their own share of the rate: a dataset
        // used by a chain at 5% is asked for a twentieth of the run's iterations.
        let share: f64 = resolved
            .chains
            .iter()
            .filter(|chain| {
                chain
                    .steps
                    .iter()
                    .any(|step| step.request.datasets().any(|read| read == index))
            })
            .map(|chain| chain.percent)
            .sum();
        if share <= 0.0 {
            continue;
        }
        let needed = (rate * share / PERCENT_TOTAL * sending.as_secs_f64()).ceil() as u64;
        require(
            dataset.rows() as u64 >= needed,
            &format!(
                "mix.json#/datasets/{}: unique_per_iteration needs one row per iteration                  and this run starts {needed} of the chains that read it, but the file                  holds {} rows",
                dataset.name(),
                dataset.rows()
            ),
        )?;
    }
    Ok(())
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
fn calibration_body_bytes(chains: &[Arc<crate::chain::Compiled>]) -> usize {
    chains
        .iter()
        .flat_map(|chain| chain.steps.iter())
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
