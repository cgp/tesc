//! The NDJSON records the engine writes and the API reads.
//!
//! **This is the contract between the two halves** (see
//! `docs/design-api-engine-contract.md` §C1). It is frozen before either side
//! implements it so that integration is wiring rather than negotiation, and so the
//! API can be built against recorded fixture streams before an engine exists.
//!
//! Two streams:
//!
//! - `--summary`: [`Record::Summary`] every 250ms, plus the lifecycle and annotation
//!   records. Small; drives the live view.
//! - `--events`: [`Record::Request`], one per request, optionally sampled. Bulky;
//!   drives drill-down.
//!
//! Every record carries `t_ms`, milliseconds since the run's monotonic start, so load
//! and host series align on one clock regardless of wall-clock skew between machines.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How serialized histograms in [`ChainStats`] are encoded.
pub const HISTOGRAM_ENCODING: &str = "hdr-v2-base64";

/// The version of this contract. Bumped when a record shape changes incompatibly;
/// the API refuses a stream whose version it does not know.
pub const EVENTS_VERSION: u32 = 1;

/// One line of NDJSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    RunStarted(RunStarted),
    PhaseChanged(PhaseChanged),
    TargetStarted(TargetStarted),
    TargetFinished(TargetFinished),
    Summary(Summary),
    Request(RequestEvent),
    Annotation(Annotation),
    RunFinished(RunFinished),
}

/// First record of every stream. Everything needed to identify and reproduce the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunStarted {
    pub events_version: u32,
    pub run_id: String,
    pub t_ms: u64,
    /// Wall clock, RFC 3339. For display only — alignment uses `t_ms`.
    pub started_at: String,
    pub engine_version: String,
    /// Hash of the plan bundle as it ran.
    pub plan_hash: String,
    pub plan_name: String,
    /// Seeds every per-VU RNG, so a run can be replayed with identical traffic.
    pub seed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_profile: Option<String>,
    /// Target ids in the order they will run.
    pub targets: Vec<String>,
    pub histogram_encoding: String,
}

/// The phased timeline. Statistics are always read against the phase that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Observe only, no traffic: initial conditions.
    Baseline,
    /// Traffic on, excluded from the summary.
    Warmup,
    /// The window the headline numbers come from.
    Measure,
    /// Traffic off, waiting for in-flight requests to complete.
    Drain,
    /// Observe only: recovery.
    Settle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PhaseChanged {
    pub t_ms: u64,
    pub target_id: String,
    pub phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetStarted {
    pub t_ms: u64,
    pub target_id: String,
    /// Position in the sweep, from 1.
    pub index: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TargetFinished {
    pub t_ms: u64,
    pub target_id: String,
    pub completed: bool,
}

/// A periodic aggregate snapshot: the live view's entire input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Summary {
    pub t_ms: u64,
    pub target_id: String,
    pub phase: Phase,
    /// Length of the window these counts cover.
    pub window_ms: u64,

    /// Offered rate for the window, in chain iterations per second.
    pub target_rate: f64,
    /// Achieved rate. The gap against `target_rate` is the headline number.
    pub achieved_rate: f64,

    pub in_flight: u32,
    /// Requests scheduled but not yet sent.
    pub queue_depth: u32,
    /// How far behind its own send schedule the generator is running. The guard
    /// against reporting the generator's limits as the target's.
    pub drift_ms: f64,

    pub bytes_sent: u64,
    pub bytes_received: u64,

    pub connections_opened: u64,
    pub connections_reused: u64,

    /// Per chain, then per step within it.
    pub chains: BTreeMap<String, ChainStats>,

    pub generator: GeneratorHealth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChainStats {
    pub iterations_started: u64,
    pub iterations_completed: u64,
    pub iterations_aborted: u64,
    /// End-to-end chain duration, which is not the sum of step medians.
    pub duration: Histogram,
    pub steps: BTreeMap<String, StepStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StepStats {
    pub attempted: u64,
    pub completed: u64,
    /// Transport and assertion failures. An expected-failure call that got the status
    /// it asserted is **not** counted here.
    pub failed: u64,
    /// Exact status codes seen, keyed by code as a string.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub statuses: BTreeMap<String, u64>,
    /// Transport error counts keyed by [`ErrorClass`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, u64>,
    /// Assertion failures keyed by the assertion's index within the call.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assertion_failures: BTreeMap<String, u64>,
    pub total: Histogram,
    pub ttfb: Histogram,
}

/// A serialized HDR histogram plus the few figures worth reading without decoding it.
///
/// Histograms are carried rather than percentiles because histograms merge and
/// percentiles do not: merging five 30s runs is what gets past the sample-count floor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Histogram {
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_us: Option<f64>,
    /// Encoded per [`HISTOGRAM_ENCODING`]. Absent when `count` is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
}

/// Self-metrics. Without these a run cannot be trusted, so they ride in every summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GeneratorHealth {
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub open_fds: u64,
    /// Tokio scheduler lag.
    pub scheduler_lag_ms: f64,
    /// Demanded rate over the calibrated ceiling, when a machine profile is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headroom_ratio: Option<f64>,
    /// Event records dropped to output backpressure. Non-zero raises an annotation.
    pub events_dropped: u64,
    /// What each named generator cost and how often it failed (§9.6). Reported apart
    /// from request latency: if generation is the slow part that has to be visible,
    /// and folding it into the response time would make the service look slow.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generation: BTreeMap<String, GenerationStats>,
}

/// One generator's own cost over a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GenerationStats {
    pub calls: u64,
    /// Calls that produced no request. Their own error class, never a target error.
    pub failed: u64,
    pub duration: Histogram,
}

/// One request. Sampled when the stream cannot keep up; `sampled` says so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestEvent {
    pub t_ms: u64,
    pub target_id: String,
    pub phase: Phase,
    pub chain: String,
    /// Which iteration of the chain this belonged to.
    pub iteration: u64,
    pub step: String,
    pub call: String,

    /// Phase breakdown. TTFB against total separates "the server is thinking" from
    /// "the response is big or the link is slow".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_us: Option<u64>,
    pub total_us: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connection_reused: bool,

    /// Absent when the request succeeded and every assertion passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RequestError>,

    /// True when this record is one of a sample rather than a complete stream.
    #[serde(default)]
    pub sampled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestError {
    pub class: ErrorClass,
    pub message: String,
    /// Index of the failed assertion within the call, when `class` is `assertion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion_index: Option<usize>,
    /// Set when the full request and response were retained as an error sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_path: Option<String>,
}

/// Failure causes, kept distinct because they mean different things. Generation and
/// extraction failures are ours, not the target's, and are never counted against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    DnsFailure,
    ConnectionRefused,
    ConnectTimeout,
    ReadTimeout,
    TlsFailure,
    ConnectionReset,
    UnexpectedEof,
    HttpStatus,
    ContentTypeMismatch,
    SchemaValidation,
    Assertion,
    /// A field a later step needed was missing: the chain broke here.
    Extraction,
    /// The generator failed to build the request. Ours, not theirs.
    Generation,
    Other,
}

/// A structured note attached to the run. Detectors emit these continuously.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Annotation {
    pub t_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Stable machine-readable code, e.g. `concurrency_cap_reached`.
    pub code: String,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    pub from_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<u64>,
    /// Written for a person reading the chart later.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// `Invalid` is not cosmetic: such a run cannot become a baseline without an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunFinished {
    pub t_ms: u64,
    /// Mirrors the process exit code: 0 pass, non-zero an SLO breach or abort.
    pub exit_code: i32,
    pub slo: Vec<SloVerdict>,
    /// Why the run ended, when it was not simply the configured duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_because: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SloVerdict {
    pub metric: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    pub passed: bool,
    pub observed: f64,
    pub threshold: f64,
    /// False when the sample count could not support the claim, in which case the
    /// verdict is advisory rather than a failure.
    pub supported: bool,
}
