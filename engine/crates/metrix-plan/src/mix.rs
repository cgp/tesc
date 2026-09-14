//! The **mix** document: which calls run, in what chains, at what share of the load.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Auth;
use crate::call::Call;
use crate::common::{Dur, Selector};

/// The load mixture and its shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    pub version: u32,
    pub name: String,

    /// Call files to load, relative to the bundle directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<PathBuf>,

    #[serde(default)]
    pub defaults: Defaults,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,

    #[serde(default)]
    pub phases: Phases,

    pub load: Load,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub datasets: BTreeMap<String, Dataset>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generators: BTreeMap<String, Generator>,

    /// The mixture proper. Percentages must total 100 — checked by validation, not here.
    pub chains: Vec<Chain>,

    #[serde(default)]
    pub engine: EngineTuning,

    #[serde(default)]
    pub capture: Capture,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe: Option<Observe>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slo: Vec<Slo>,
}

/// Applied to every call unless the call overrides them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<bool>,
}

/// The observe-only windows either side of the traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Phases {
    /// Observe only, no traffic: initial conditions.
    pub baseline: Dur,
    /// Observe only, after traffic stops: recovery.
    pub settle: Dur,
}

impl Default for Phases {
    /// On by default. The cost is wall clock; the benefit is that the numbers mean something.
    fn default() -> Self {
        Self {
            baseline: Dur::from_secs(30),
            settle: Dur::from_secs(60),
        }
    }
}

/// How much load, in what shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Load {
    #[serde(default)]
    pub mode: LoadMode,

    pub duration: Dur,

    /// Measured separately and excluded from the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup: Option<Dur>,

    #[serde(default)]
    pub model: LoadModel,

    /// Total chain iterations per second, split across chains by percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,

    /// Safety cap on in-flight requests. Hitting it annotates the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,

    /// Mode `stages`: a hand-specified ramp.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<Stage>,

    /// Mode `breakpoint`: ramp until something gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakpoint: Option<Breakpoint>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    #[default]
    Fixed,
    Stages,
    Breakpoint,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LoadModel {
    /// Fixed arrival rate: requests are issued on schedule regardless of outstanding ones.
    #[default]
    Open,
    /// Fixed concurrency: a slow response delays the next request.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub duration: Dur,
    pub rate: f64,
}

/// Breakpoint search parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Breakpoint {
    pub start_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_factor: Option<f64>,
    pub step_duration: Dur,
    /// Idle between steps so queues drain; without it a step inherits the previous
    /// step's backlog and the search finds a false early cliff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_recovery: Option<Dur>,
    /// Always required: the search must have a ceiling.
    pub max_rate: f64,
    #[serde(default)]
    pub refine: bool,
    #[serde(default)]
    pub stop_on: StopOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopOn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_multiple_of_baseline: Option<f64>,
    /// Achieved below target by this much means saturation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_shortfall_pct: Option<f64>,
}

/// A named sequence of calls, and its share of the traffic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    pub name: String,

    /// Share of the total rate. Percentages across all chains must total 100.
    /// Buys chain *iterations*, not requests: 20% of 150/s over a two-step chain
    /// is 30 iterations/s and 60 req/s.
    pub percent: f64,

    #[serde(default)]
    pub session: SessionPolicy,

    /// Required by, and only meaningful for, `session: "pool"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_size: Option<u32>,

    pub steps: Vec<Step>,
}

/// Whether the chain needs a fresh session. A property of the behavior being
/// modelled, not of the service. Binds cookie jar and auth identity together.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionPolicy {
    /// New auth identity and empty cookie jar per iteration: a first-time user.
    Fresh,
    /// One session for the life of the virtual user: a returning user.
    #[default]
    Reuse,
    /// A fixed set of sessions cycled across iterations: a realistic population.
    Pool,
}

/// One call within a chain, with a stable id independent of which call it invokes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Stable key for chart series, error reports, and SLOs.
    pub id: String,

    /// Name of a call defined in one of the loaded call files.
    pub call: String,

    /// Shallow overrides of the call's fields. Discouraged: two genuinely different
    /// requests should be two calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<Box<Call>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnFailure>,

    /// The async-job pattern: POST returns 202, poll until done. Polling time is
    /// recorded separately so it does not contaminate request latency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_until: Option<RepeatUntil>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// Record the chain as failed at this step. Aborted chains are counted separately
    /// from failed requests.
    #[default]
    Abort,
    Continue,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepeatUntil {
    #[serde(flatten)]
    pub selector: Selector,
    pub equals: Value,
    pub max_attempts: u32,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub file: PathBuf,
    #[serde(default)]
    pub mode: DatasetMode,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DatasetMode {
    #[default]
    RoundRobin,
    Random,
    /// Matters for POSTs that must not collide.
    UniquePerIteration,
}

/// Static files a generator may read, as a directory inside the bundle.
///
/// A ceiling rather than a hope: the whole point of allowing reads is convenience at
/// setup, not I/O during the measured window, so everything under `dir` is loaded
/// before the run and a plan pointing at something too large is refused rather than
/// swallowing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub dir: PathBuf,
    /// Total kilobytes allowed under `dir`. Default 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kb: Option<u64>,
}

/// How a request gets built when a template is not enough.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Generator {
    /// The default tier: embedded, in-process, one VM per worker thread.
    Lua {
        file: PathBuf,
        #[serde(default = "default_lua_entry")]
        entry: String,
        /// Static files the script reads: sample payloads, a fixture set, a word
        /// list. Loaded once at run start, never read per call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        corpus: Option<Corpus>,
        /// Build a request buffer during warmup so the measured window pays nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefetch: Option<u32>,
    },
    /// Compiled in-tree and registered by name: large XML, signing, compression.
    Plugin {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefetch: Option<u32>,
    },
    /// The escape hatch: a script that already exists.
    Exec {
        command: Vec<String>,
        #[serde(default)]
        protocol: ExecProtocol,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pool: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefetch: Option<u32>,
    },
}

fn default_lua_entry() -> String {
    "generate".to_owned()
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecProtocol {
    /// A pool of long-lived processes speaking line-delimited JSON.
    #[default]
    Ndjson,
    /// Fork per call. Honest about its cost, rate-capped, flagged in the UI.
    Oneshot,
}

/// Generator-side tuning. Raising `worker_threads` is the first lever for headroom.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EngineTuning {
    /// Default: physical cores minus one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_threads: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections_per_host: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_cores: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 99))]
    pub rate_tolerance_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3600000))]
    pub send_drift_threshold_ms: Option<u64>,
    /// Permit a run above the calibrated generator ceiling. The run is marked invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_generator_limited: Option<bool>,
}

/// What to keep from failed calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// First N errored calls retained in full, **per error class** — otherwise one
    /// flood of connection-refused evicts the single 500 that explains the problem.
    pub error_samples: u32,
    pub body_max_kb: u32,
    /// Header and field names to redact before anything is written down.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact: Vec<String>,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            error_samples: 10,
            body_max_kb: 64,
            redact: vec!["Authorization".to_owned(), "Set-Cookie".to_owned()],
        }
    }
}

/// Host-side collection during the run. The observer is API-side; this only says
/// what to ask it for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observe {
    pub interval_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collect: Vec<String>,
}

/// A threshold the run is judged against. Evaluated at the end; sets the exit code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Slo {
    pub metric: String,
    /// Scoped to one chain, or run-wide when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
}
