//! Bounded output queues. Only writer threads construct and serialize request records.

use crate::{Failure, Plan, http::Observation};
use chrono::{SecondsFormat, Utc};
use metrix_metrics::{
    Record,
    aggregation::{Accumulator, ArrivalTiming, Window},
    events::*,
    stats::percentiles,
};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as completion,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const RESERVED: usize = 16;
// Histogram windows are much larger than scalar request packets.
const SUMMARY_CAPACITY: usize = 32;
const EVENTS_CAPACITY: usize = 1024;

#[derive(Default)]
struct Losses {
    summaries: AtomicU64,
    requests: AtomicU64,
    sampled: AtomicU64,
    failed: AtomicBool,
}

#[derive(Clone)]
struct Identity {
    target: String,
    /// Every chain in the mixture. A run-wide annotation covers all of them, and
    /// naming one would be naming the wrong one as soon as there are two.
    chains: Vec<String>,
    rate: f64,
    headroom_ratio: Option<f64>,
}

/// A step that was attempted and never reached the wire.
pub(crate) struct NotSent<'a> {
    pub chain: &'static str,
    pub step: &'static str,
    pub cause: metrix_metrics::aggregation::Cause,
    /// The variable that was missing, or what the generator said.
    pub detail: &'a str,
    pub truncated: bool,
}

struct RequestData {
    identity: Arc<Identity>,
    /// Which chain and step this request was. Per record rather than from the run's
    /// identity: a mixture sends several chains, and a record that named the wrong
    /// one would be worse than one that named none.
    chain: &'static str,
    step: &'static str,
    call: &'static str,
    t_ms: u64,
    phase: Phase,
    iteration: u64,
    ttfb_us: Option<u64>,
    total_us: u64,
    status: Option<u16>,
    bytes_sent: u64,
    bytes_received: u64,
    connection_reused: bool,
    error: Option<Failure>,
    cancelled: bool,
}

enum Packet {
    Fence(completion::Sender<()>),
    Record(Arc<Identity>, Box<Record>),
    Summary {
        identity: Arc<Identity>,
        t_ms: u64,
        phase: Phase,
        window: Box<Window>,
        diagnostics: Box<crate::Diagnostics>,
        closing: bool,
        partial: bool,
    },
    Request(RequestData),
    Percentiles {
        identity: Arc<Identity>,
        t_ms: u64,
        from_ms: u64,
        partial: bool,
        metrics: Box<Accumulator>,
    },
}

impl Packet {
    fn identity(&self) -> Option<&Arc<Identity>> {
        match self {
            Self::Fence(_) => None,
            Self::Record(identity, _)
            | Self::Summary { identity, .. }
            | Self::Percentiles { identity, .. } => Some(identity),
            Self::Request(data) => Some(&data.identity),
        }
    }
    fn time(&self) -> u64 {
        match self {
            Self::Fence(_) => unreachable!("fences do not produce records"),
            Self::Summary { t_ms, .. } => *t_ms,
            Self::Percentiles { t_ms, .. } => *t_ms,
            Self::Request(request) => request.t_ms,
            Self::Record(_, record) => match record.as_ref() {
                Record::RunStarted(r) => r.t_ms,
                Record::PhaseChanged(r) => r.t_ms,
                Record::TargetStarted(r) => r.t_ms,
                Record::TargetFinished(r) => r.t_ms,
                Record::Annotation(r) => r.t_ms,
                Record::RunFinished(r) => r.t_ms,
                Record::ErrorSample(r) => r.t_ms,
                _ => unreachable!("data records use typed packets"),
            },
        }
    }
}

struct Stream {
    sender: mpsc::Sender<Packet>,
    done: completion::Receiver<io::Result<()>>,
}

/// Output delivery statistics remain available even when the consumer cannot read annotations.
#[derive(Debug, Default)]
pub struct OutputReport {
    pub summaries_dropped: u64,
    pub events_dropped: u64,
    pub events_sampled_out: u64,
    pub writers_unfinished: u64,
    pub failed: bool,
}

/// Created once before execution. No I/O or request serialization occurs on its send path.
pub struct Output {
    slo: std::cell::RefCell<Vec<SloVerdict>>,
    summary: Stream,
    events: Option<Stream>,
    identity: std::cell::RefCell<Arc<Identity>>,
    losses: Arc<Losses>,
    start: Instant,
    sample_rate: f64,
    seed: u64,
    /// Whether the unbound-variable note has been written. A plan error repeats
    /// every iteration, and saying it a hundred thousand times buries it.
    said_unbound: AtomicBool,
}

impl Output {
    pub fn open(
        plan: &Plan,
        summary: &Path,
        events: Option<&Path>,
        sample_rate: f64,
        seed: u64,
    ) -> Result<Self, String> {
        if !sample_rate.is_finite() || !(0.0..=1.0).contains(&sample_rate) {
            return Err("--sample-rate: expected a finite number in [0, 1]".into());
        }
        if let Some(events) = events {
            if destination(summary)? == destination(events)? {
                return Err("--summary and --events must have different destinations".into());
            }
        }
        let summary_writer = writer(summary).map_err(|_| "--summary: cannot create output file")?;
        let events_writer = events
            .map(writer)
            .transpose()
            .map_err(|_| "--events: cannot create output file")?;
        let identity = Arc::new(Identity {
            target: plan.target.id.clone(),
            chains: plan.chains.iter().map(|c| c.name.to_owned()).collect(),
            rate: plan.rate,
            headroom_ratio: plan.headroom_ratio,
        });
        let losses = Arc::new(Losses::default());
        let start = Instant::now();
        let summary = spawn_writer(
            summary_writer,
            SUMMARY_CAPACITY,
            Arc::clone(&identity),
            Arc::clone(&losses),
            sample_rate < 1.0,
        )?;
        let events = events_writer
            .map(|writer| {
                spawn_writer(
                    writer,
                    EVENTS_CAPACITY,
                    Arc::clone(&identity),
                    Arc::clone(&losses),
                    sample_rate < 1.0,
                )
            })
            .transpose()?;
        let output = Self {
            slo: std::cell::RefCell::new(Vec::new()),
            summary,
            events,
            identity: std::cell::RefCell::new(identity),
            losses,
            start,
            sample_rate,
            seed,
            said_unbound: AtomicBool::new(false),
        };
        let now = Utc::now();
        output.lifecycle(Record::RunStarted(RunStarted {
            events_version: EVENTS_VERSION,
            run_id: format!(
                "{}-{}",
                std::process::id(),
                now.timestamp_nanos_opt().unwrap_or(now.timestamp_micros())
            ),
            t_ms: 0,
            started_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            plan_hash: plan.hash.clone(),
            plan_name: plan.name.clone(),
            seed,
            machine_profile: plan
                .machine_profile
                .as_ref()
                .map(|profile| profile.id.clone()),
            targets: plan
                .target_order()
                .iter()
                .map(|&i| plan.targets.list[i].id.clone())
                .collect(),
            histogram_encoding: HISTOGRAM_ENCODING.into(),
        }));
        if let Some(chain) = plan.narrowed_to() {
            // Said out loud, because a run of one chain out of six is not a run of
            // the mixture, and a stored run that did not say so would be compared
            // against ones that were.
            output.lifecycle(Record::Annotation(Annotation {
                t_ms: 0,
                target_id: None,
                code: "single_chain".into(),
                severity: Severity::Invalid,
                phase: None,
                from_ms: 0,
                to_ms: None,
                message: format!(
                    "--chain ran {chain:?} alone at the whole rate. These numbers are about                      that chain and not about the mixture the plan declares."
                ),
                detail: Some(serde_json::json!({"chain": chain})),
            }));
        }
        output.lifecycle(Record::Annotation(Annotation {
            t_ms: 0, target_id: None, code: "self_metrics_unavailable".into(), severity: Severity::Info,
            phase: None, from_ms: 0, to_ms: None,
            message: "Zero in cpu_pct, rss_bytes and open_fds means unavailable; OS resource probes are not implemented yet.".into(),
            detail: Some(serde_json::json!({"unavailable": ["cpu_pct", "rss_bytes", "open_fds"]})),
        }));
        let planned = plan.duration.as_secs_f64() * plan.rate;
        if planned < metrix_plan::MIN_SAMPLES as f64 {
            output.lifecycle(Record::Annotation(Annotation {
                t_ms: 0, target_id: Some(plan.target.id.clone()), code: "planned_sample_count_low".into(), severity: Severity::Warn,
                phase: Some(Phase::Measure), from_ms: 0, to_ms: None,
                message: "Planned measured volume is below 2250 requests; actual histogram counts determine percentile support.".into(),
                detail: Some(serde_json::json!({"planned_samples": planned, "minimum_samples": metrix_plan::MIN_SAMPLES})),
            }));
        }
        if let (Some(ratio), Some(profile)) = (plan.headroom_ratio, plan.machine_profile.as_ref()) {
            let severity = if ratio > 0.9 {
                Severity::Invalid
            } else if ratio > 0.7 {
                Severity::Warn
            } else {
                Severity::Info
            };
            output.lifecycle(Record::Annotation(Annotation {
                t_ms: 0,
                target_id: Some(plan.target.id.clone()),
                code: "generator_headroom".into(),
                severity,
                phase: None,
                from_ms: 0,
                to_ms: None,
                message: if ratio > 0.9 {
                    "Demand exceeds 90% of the calibrated generator ceiling; this overridden run is invalid for target capacity claims."
                } else if ratio > 0.7 {
                    "Demand uses more than 70% of the calibrated generator ceiling."
                } else if ratio >= 0.5 {
                    "Demand uses at least half of the calibrated generator ceiling."
                } else {
                    "Demand is below half of the calibrated generator ceiling."
                }.into(),
                detail: Some(serde_json::json!({
                    "demand_rps": plan.rate,
                    "ceiling_rps": profile.ceiling(plan.worker_threads),
                    "headroom_ratio": ratio,
                    "machine_profile": profile.id,
                    "overridden": plan.allow_generator_limited,
                })),
            }));
        }
        Ok(output)
    }

    pub(crate) fn elapsed(&self) -> u64 {
        millis(self.start.elapsed())
    }

    /// Calculate final statistics on the writer thread, after the load path stops.
    pub(crate) fn percentiles(&self, metrics: &Accumulator, from_ms: u64, partial: bool) {
        let t_ms = self.elapsed();
        if self
            .summary
            .sender
            .try_send(Packet::Percentiles {
                identity: self.identity.borrow().clone(),
                t_ms,
                from_ms: from_ms.min(t_ms),
                partial,
                metrics: Box::new(metrics.clone()),
            })
            .is_err()
        {
            self.losses.failed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn arrival_totals(&self, measure: &ArrivalTiming, warmup: &ArrivalTiming) {
        self.note("arrival_timing_total", if measure.telemetry_dropped + warmup.telemetry_dropped > 0 { Severity::Warn } else { Severity::Info }, serde_json::json!({"measure":arrival_evidence(measure),"warmup":arrival_evidence(warmup)}));
    }

    pub(crate) fn step_start(&self, plan: &Plan) {
        let identity = Arc::new(Identity {
            rate: plan.rate,
            headroom_ratio: plan.headroom_ratio,
            ..self.identity.borrow().as_ref().clone()
        });
        *self.identity.borrow_mut() = identity;
    }
    pub(crate) fn target_start(&self, plan: &Plan, index: usize) {
        let identity = Arc::new(Identity {
            target: plan.target.id.clone(),
            rate: plan.rate,
            headroom_ratio: plan.headroom_ratio,
            ..self.identity.borrow().as_ref().clone()
        });
        *self.identity.borrow_mut() = identity;
        self.lifecycle(Record::TargetStarted(TargetStarted {
            t_ms: self.elapsed(),
            target_id: plan.target.id.clone(),
            index: index as u32 + 1,
            total: plan.targets.list.len() as u32,
        }));
        self.note("target_metadata", Severity::Info, serde_json::json!({"attributes": plan.target.attributes, "address": plan.target.address, "host_header": plan.target.host_header, "sni": plan.target.tls.sni}));
        if plan.observe.is_some() {
            self.note(
                "observation_unavailable",
                Severity::Info,
                serde_json::json!({"owner":"API observer"}),
            );
        }
        if plan.target.tls.insecure_skip_verify {
            self.note(
                "insecure_skip_verify",
                Severity::Warn,
                serde_json::json!({"certificate_verification": "disabled"}),
            );
        }
    }
    pub(crate) fn note(&self, code: &str, severity: Severity, detail: serde_json::Value) {
        let t_ms = self.elapsed();
        self.lifecycle(Record::Annotation(Annotation {
            t_ms: self.elapsed(),
            target_id: Some(self.identity.borrow().target.clone()),
            code: code.into(),
            severity,
            phase: None,
            from_ms: t_ms,
            to_ms: None,
            message: code.replace('_', " "),
            detail: Some(detail),
        }));
    }
    pub(crate) fn set_slo(&self, verdicts: &[SloVerdict]) {
        *self.slo.borrow_mut() = verdicts.to_vec();
    }
    pub(crate) fn target_finish(&self, completed: bool) {
        self.lifecycle(Record::TargetFinished(TargetFinished {
            t_ms: self.elapsed(),
            target_id: self.identity.borrow().target.clone(),
            completed,
        }));
    }
    fn lifecycle(&self, record: Record) {
        // Reserved slots favor lifecycle records; prolonged backpressure can still drop them.
        for stream in std::iter::once(&self.summary).chain(self.events.iter()) {
            if stream
                .sender
                .try_send(Packet::Record(
                    self.identity.borrow().clone(),
                    Box::new(record.clone()),
                ))
                .is_err()
            {
                self.losses.failed.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn phase(&self, phase: Phase) {
        self.lifecycle(Record::PhaseChanged(PhaseChanged {
            t_ms: self.elapsed(),
            target_id: self.identity.borrow().target.clone(),
            phase,
        }));
    }

    pub(crate) fn summary(
        &self,
        window: &Window,
        phase: Phase,
        diagnostics: &crate::Diagnostics,
        closing: bool,
        partial: bool,
    ) {
        if self.summary.sender.capacity() <= RESERVED
            || self
                .summary
                .sender
                .try_send(Packet::Summary {
                    identity: self.identity.borrow().clone(),
                    t_ms: self.elapsed(),
                    phase,
                    window: Box::new(window.clone()),
                    diagnostics: Box::new(*diagnostics),
                    closing,
                    partial,
                })
                .is_err()
        {
            self.losses.summaries.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn selected(&self, iteration: u64) -> bool {
        if self.sample_rate == 1.0 {
            return true;
        }
        if self.sample_rate == 0.0 {
            return false;
        }
        // SplitMix64 finalizer: deterministic, allocation-free iteration selection.
        let mut value = iteration
            .wrapping_add(self.seed)
            .wrapping_add(0x9e3779b97f4a7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^= value >> 31;
        ((value >> 11) as f64) / ((1u64 << 53) as f64) < self.sample_rate
    }

    fn request_data(&self, data: RequestData) {
        let Some(events) = &self.events else {
            return;
        };
        if !self.selected(data.iteration) {
            self.losses.sampled.fetch_add(1, Ordering::Relaxed);
        } else if events.sender.capacity() <= RESERVED
            || events.sender.try_send(Packet::Request(data)).is_err()
        {
            self.losses.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn request(
        &self,
        iteration: u64,
        phase: Phase,
        chain: &'static str,
        step: &'static str,
        call: &'static str,
        observation: &Observation,
    ) {
        self.request_data(RequestData {
            identity: self.identity.borrow().clone(),
            t_ms: self.elapsed(),
            phase,
            chain,
            step,
            call,
            iteration,
            ttfb_us: observation.ttfb.map(micros),
            total_us: micros(observation.request_duration.unwrap_or(observation.total)),
            status: observation.status,
            bytes_sent: if observation.sent.is_some() {
                observation.bytes_sent
            } else {
                0
            },
            bytes_received: observation.bytes_received,
            connection_reused: observation.connection_reused,
            error: observation.error,
            cancelled: false,
        });
    }

    pub(crate) fn cancel(
        &self,
        iteration: u64,
        phase: Phase,
        chain: &'static str,
        elapsed: Duration,
    ) {
        self.request_data(RequestData {
            identity: self.identity.borrow().clone(),
            t_ms: self.elapsed(),
            phase,
            chain,
            // A cancelled iteration was cut short in flight; which step it was in is
            // not something the scheduler tracks, and guessing would be worse.
            step: "",
            call: "",
            iteration,
            ttfb_us: None,
            total_us: micros(elapsed),
            status: None,
            bytes_sent: 0,
            bytes_received: 0,
            connection_reused: false,
            error: None,
            cancelled: true,
        });
    }

    /// A step that could not be built, said once.
    ///
    /// Once, not once per iteration: a variable nothing captures is missing every
    /// single time, and a hundred thousand identical notes would bury the fact that
    /// explains all of them. The per-step failure count carries how often. A
    /// generator that failed is said once for the same reason — a script with a bug
    /// in it has the same bug on every call.
    /// One errored call, kept in full (§9.3).
    ///
    /// On the events stream only. The summary stream feeds live charts and is read at
    /// a quarter-second cadence; a sample carries request and response bodies, and
    /// putting those on that stream would be sending bodies to a chart.
    pub(crate) fn error_sample(&self, sample: metrix_metrics::events::ErrorSample) {
        let Some(events) = &self.events else {
            return;
        };
        if events
            .sender
            .try_send(Packet::Record(
                self.identity.borrow().clone(),
                Box::new(Record::ErrorSample(sample)),
            ))
            .is_err()
        {
            self.losses.failed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn not_sent(&self, iteration: u64, phase: Phase, what: NotSent<'_>) {
        let NotSent {
            chain,
            step,
            cause,
            detail,
            truncated,
        } = what;
        let generation = cause == metrix_metrics::aggregation::Cause::Generation;
        self.request_data(RequestData {
            identity: self.identity.borrow().clone(),
            t_ms: self.elapsed(),
            phase,
            chain,
            step,
            call: "",
            iteration,
            ttfb_us: None,
            total_us: 0,
            status: None,
            bytes_sent: 0,
            bytes_received: 0,
            connection_reused: false,
            error: Some(Failure::Send),
            cancelled: false,
        });
        if self.said_unbound.swap(true, Ordering::Relaxed) {
            return;
        }
        let message = if generation {
            format!(
                "step {step:?} of chain {chain:?} could not be built: {detail}. Nothing was                  sent, so this is the plan's own failure and not the service's"
            )
        } else {
            format!(
                "step {step:?} of chain {chain:?} reads {{{{ {detail} }}}}, which no                  response before it provided; the chain stops here every iteration{}",
                if truncated {
                    " (a response was cut at the capture ceiling, so an extractor may                      have been looking past the cut)"
                } else {
                    ""
                }
            )
        };
        self.lifecycle(Record::Annotation(Annotation {
            t_ms: self.elapsed(),
            target_id: Some(self.identity.borrow().target.clone()),
            code: if generation {
                "generation_failed".into()
            } else {
                "chain_unbound".into()
            },
            severity: Severity::Invalid,
            phase: Some(phase),
            from_ms: 0,
            to_ms: None,
            message,
            detail: Some(if generation {
                serde_json::json!({ "chain": chain, "step": step, "reason": detail })
            } else {
                serde_json::json!({ "chain": chain, "step": step, "variable": detail })
            }),
        }));
    }

    /// Wait once with a shared deadline. Stalled writers are detached, never joined.
    pub fn finish(self, mut exit_code: i32, stopped_because: Option<String>) -> OutputReport {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut fences = Vec::new();
        for stream in std::iter::once(&self.summary).chain(self.events.iter()) {
            let (sender, receiver) = completion::channel();
            if stream.sender.try_send(Packet::Fence(sender)).is_err() {
                self.losses.failed.store(true, Ordering::Relaxed);
            }
            fences.push(receiver);
        }
        // Discover blocked readers before reporting the final exit code to healthy ones.
        for fence in fences {
            if fence
                .recv_timeout(
                    (deadline - Duration::from_millis(50))
                        .saturating_duration_since(Instant::now()),
                )
                .is_err()
            {
                self.losses.failed.store(true, Ordering::Relaxed);
            }
        }
        // A recording that could not be written outranks anything the run concluded,
        // because the conclusion is what could not be written down (§16). Only an
        // interruption outranks it: the reader asked for the run to end.
        if self.losses.failed.load(Ordering::Relaxed) && exit_code != 130 {
            exit_code = 1;
        }
        let t_ms = self.elapsed();
        self.lifecycle(Record::RunFinished(RunFinished {
            t_ms,
            exit_code,
            slo: self.slo.borrow().clone(),
            stopped_because,
        }));
        let Self {
            summary,
            events,
            losses,
            ..
        } = self;
        let mut unfinished = 0;
        for Stream { sender, done } in std::iter::once(summary).chain(events) {
            drop(sender);
            match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(completion::RecvTimeoutError::Disconnected) => {
                    losses.failed.store(true, Ordering::Relaxed);
                }
                Err(completion::RecvTimeoutError::Timeout) => unfinished += 1,
            }
        }
        OutputReport {
            summaries_dropped: losses.summaries.load(Ordering::Relaxed),
            events_dropped: losses.requests.load(Ordering::Relaxed),
            events_sampled_out: losses.sampled.load(Ordering::Relaxed),
            writers_unfinished: unfinished,
            failed: losses.failed.load(Ordering::Relaxed) || unfinished > 0,
        }
    }
}

fn destination(path: &Path) -> Result<PathBuf, String> {
    if path == Path::new("-") {
        return Ok(PathBuf::from("-"));
    }
    let absolute = std::path::absolute(path).map_err(|_| "output: invalid destination")?;
    let parent = absolute
        .parent()
        .ok_or("output: invalid destination")?
        .canonicalize()
        .map_err(|_| "output: destination directory does not exist")?;
    let path = parent.join(absolute.file_name().ok_or("output: invalid destination")?);
    // Windows paths are case-insensitive. Existing aliases are also rejected by create_new.
    if cfg!(windows) {
        Ok(PathBuf::from(path.to_string_lossy().to_lowercase()))
    } else {
        Ok(path)
    }
}

fn writer(path: &Path) -> io::Result<Box<dyn Write + Send>> {
    if path == Path::new("-") {
        Ok(Box::new(io::stdout()))
    } else {
        Ok(Box::new(
            OpenOptions::new().write(true).create_new(true).open(path)?,
        ))
    }
}

fn spawn_writer(
    writer: Box<dyn Write + Send>,
    capacity: usize,
    identity: Arc<Identity>,
    losses: Arc<Losses>,
    sampled: bool,
) -> Result<Stream, String> {
    let (sender, receiver) = mpsc::channel(capacity);
    let (done_sender, done) = completion::channel();
    std::thread::Builder::new()
        .name("metrix-output".into())
        .spawn(move || {
            let result = write_stream(writer, receiver, identity, &losses, sampled);
            if result.is_err() {
                losses.failed.store(true, Ordering::Relaxed);
            }
            let _ = done_sender.send(result);
        })
        .map_err(|_| "cannot create output writer")?;
    Ok(Stream { sender, done })
}

fn write_stream(
    mut writer: Box<dyn Write + Send>,
    mut receiver: mpsc::Receiver<Packet>,
    mut identity: Arc<Identity>,
    losses: &Losses,
    sampled: bool,
) -> io::Result<()> {
    let mut buffer = Vec::with_capacity(4096);
    let mut last_losses = (0, 0, 0);
    let mut started = false;
    let mut detectors = [crate::detectors::Seen::default(); 2];
    while let Some(packet) = receiver.blocking_recv() {
        if let Some(source) = packet.identity() {
            if !Arc::ptr_eq(&identity, source) {
                identity = Arc::clone(source);
                detectors = [crate::detectors::Seen::default(); 2];
            }
        }
        let packet = match packet {
            Packet::Fence(sender) => {
                let _ = sender.send(());
                continue;
            }
            packet => packet,
        };
        let current = (
            losses.summaries.load(Ordering::Relaxed),
            losses.requests.load(Ordering::Relaxed),
            losses.sampled.load(Ordering::Relaxed),
        );
        if started && current != last_losses {
            let (code, message, severity) = if current.0 != last_losses.0
                || current.1 != last_losses.1
            {
                (
                    "events_dropped",
                    "Output backpressure dropped records; affected streams are incomplete.",
                    Severity::Warn,
                )
            } else {
                (
                    "events_sampled",
                    "Request records were intentionally sampled; summaries retain all observations.",
                    Severity::Info,
                )
            };
            write_record(
                &mut writer,
                &mut buffer,
                &Record::Annotation(Annotation {
                    t_ms: packet.time(),
                    target_id: Some(identity.target.clone()),
                    code: code.into(),
                    severity,
                    phase: None,
                    from_ms: 0,
                    to_ms: Some(packet.time()),
                    message: message.into(),
                    detail: Some(
                        serde_json::json!({"summaries_dropped": current.0, "events_dropped": current.1, "events_sampled_out": current.2}),
                    ),
                }),
            )?;
            last_losses = current;
        }
        let record = match packet {
            Packet::Fence(_) => unreachable!("fences handled above"),
            Packet::Record(_, record) => *record,
            Packet::Percentiles {
                identity: _,
                t_ms,
                from_ms,
                partial,
                metrics,
            } => {
                let raw = [
                    percentiles(&metrics.chain),
                    percentiles(&metrics.total),
                    percentiles(&metrics.ttfb),
                ];
                let mut suppressed = Vec::new();
                for (name, p) in ["chain_duration", "request_total", "ttfb"]
                    .into_iter()
                    .zip(&raw)
                {
                    for (q, p) in [
                        ("p50", &p.p50),
                        ("p95", &p.p95),
                        ("p99", &p.p99),
                        ("p99.9", &p.p99_9),
                    ] {
                        if p.support == metrix_metrics::stats::Support::Suppressed {
                            suppressed.push(serde_json::json!({"distribution": name, "percentile": q, "count": p.count, "overflow": p.overflow, "reason": p.suppression}));
                        }
                    }
                }
                if !suppressed.is_empty() {
                    write_record(&mut writer, &mut buffer, &Record::Annotation(Annotation {
                        t_ms, target_id: Some(identity.target.clone()), code: "sample_count_low".into(), severity: Severity::Warn,
                        phase: Some(Phase::Measure), from_ms, to_ms: Some(t_ms),
                        message: "Raw latency percentiles were withheld because their recorded population cannot support them.".into(),
                        detail: Some(serde_json::json!({"partial": partial, "suppressed": suppressed})),
                    }))?;
                }
                Record::Annotation(Annotation {
                t_ms, target_id: Some(identity.target.clone()), code: "load_percentiles".into(), severity: Severity::Info,
                phase: Some(Phase::Measure), from_ms, to_ms: Some(t_ms),
                message: "Measured latency percentiles with actual sample counts and binomial order-statistic 95% intervals; warmup is excluded. Intervals assume independent stationary samples.".into(),
                detail: Some(serde_json::json!({
                    "partial": partial, "chains": identity.chains,
                    "chain_duration": raw[0],
                    "request_total": raw[1],
                    "ttfb": raw[2],
                    "schedule_corrected": {
                        "method": "scheduled_arrival", "synthetic_samples": 0, "includes_skipped_arrivals": false,
                        "chain_duration": percentiles(&metrics.corrected_chain),
                        "request_total": percentiles(&metrics.corrected_total),
                        "ttfb": percentiles(&metrics.corrected_ttfb),
                    },
                })),
            })
            }
            Packet::Summary {
                identity: _,
                t_ms,
                phase,
                mut window,
                diagnostics,
                closing,
                partial,
            } => {
                let base = t_ms.saturating_sub(millis(window.to));
                for (index, (health, traffic_phase)) in [
                    (diagnostics.warmup, Phase::Warmup),
                    (diagnostics.measure, Phase::Measure),
                ]
                .into_iter()
                .enumerate()
                {
                    for annotation in crate::detectors::annotations(
                        health,
                        *diagnostics,
                        traffic_phase,
                        (base, t_ms),
                        closing,
                        partial,
                        &mut detectors[index],
                    ) {
                        let mut annotation = annotation;
                        annotation.target_id = Some(identity.target.clone());
                        write_record(&mut writer, &mut buffer, &Record::Annotation(annotation))?;
                    }
                }
                write_summary(
                    &mut writer,
                    &mut buffer,
                    &identity,
                    t_ms,
                    phase,
                    &window,
                    current.1,
                )?;
                if let Some(metrics) = window.warmup_metrics.take() {
                    window.metrics = *metrics;
                    window.in_flight = window.warmup_in_flight;
                    window.queue_depth = window.warmup_queue_depth;
                    window.scheduler_lag = Duration::ZERO;
                    window.scheduler_lag_samples = 0;
                    window.arrival = None;
                    write_summary(
                        &mut writer,
                        &mut buffer,
                        &identity,
                        t_ms,
                        Phase::Warmup,
                        &window,
                        current.1,
                    )?;
                }
                started = true;
                continue;
            }
            Packet::Request(data) => Record::Request(RequestEvent {
                t_ms: data.t_ms,
                target_id: identity.target.clone(),
                phase: data.phase,
                chain: data.chain.to_owned(),
                iteration: data.iteration,
                step: data.step.to_owned(),
                call: data.call.to_owned(),
                dns_us: None,
                connect_us: None,
                tls_us: None,
                ttfb_us: data.ttfb_us,
                total_us: data.total_us,
                status: data.status,
                bytes_sent: data.bytes_sent,
                bytes_received: data.bytes_received,
                connection_reused: data.connection_reused,
                error: if data.cancelled {
                    Some(RequestError {
                        class: ErrorClass::Other,
                        message: "request cancelled".into(),
                        assertion_index: None,
                        sample_path: None,
                    })
                } else {
                    data.error.map(|failure| RequestError {
                        class: error_class(failure),
                        message: error_message(failure).into(),
                        assertion_index: None,
                        sample_path: None,
                    })
                },
                sampled: sampled || current.1 > 0,
            }),
        };
        write_record(&mut writer, &mut buffer, &record)?;
        started = true;
    }
    writer.flush()
}

fn arrival_evidence(timing: &ArrivalTiming) -> serde_json::Value {
    serde_json::json!({
        "timer_wake_lateness": {"histogram":timing.timer_wake_lateness.snapshot(),"overflow":timing.timer_wake_lateness.overflow,"percentiles":percentiles(&timing.timer_wake_lateness)},
        "wake_to_dispatch": {"histogram":timing.wake_to_dispatch.snapshot(),"overflow":timing.wake_to_dispatch.overflow,"percentiles":percentiles(&timing.wake_to_dispatch)},
        "timer_wakes":timing.timer_wakes,"coalesced_wakes":timing.coalesced_wakes,"telemetry_dropped":timing.telemetry_dropped,
        "complete":timing.telemetry_dropped == 0,"timer_skipped_arrivals":timing.timer_skipped_arrivals,
        "dispatches":timing.dispatches,"dispatches_with_skips":timing.dispatches_with_skips,"skipped_arrivals":timing.skipped_arrivals,
        "max_skipped_per_dispatch":timing.max_skipped_per_dispatch,"phase_end_skipped":timing.phase_end_skipped,
        "skipped_per_dispatch":{"counts":timing.skipped_per_dispatch,"last_bucket":"31_or_more"}
    })
}

fn write_summary(
    writer: &mut dyn Write,
    buffer: &mut Vec<u8>,
    identity: &Identity,
    t_ms: u64,
    phase: Phase,
    window: &Window,
    dropped: u64,
) -> io::Result<()> {
    if let Some(timing) = &window.arrival {
        write_record(writer, buffer, &Record::Annotation(Annotation {
            t_ms, target_id: Some(identity.target.clone()), code: "arrival_timing".into(),
            severity: if timing.telemetry_dropped > 0 { Severity::Warn } else { Severity::Info },
            phase: Some(phase), from_ms: t_ms.saturating_sub(millis(window.to.saturating_sub(window.from))), to_ms: Some(t_ms),
            message: "Separates timer wake lateness from delay reaching arrival dispatch; coalesced wakes are sampled and lost timing evidence is counted. Percentile intervals assume independent stationary samples.".into(),
            detail: Some(arrival_evidence(timing)),
        }))?;
    }
    write_record(writer, buffer, &Record::Annotation(Annotation {
        t_ms, target_id: Some(identity.target.clone()), code: "schedule_corrected_latency".into(), severity: Severity::Info,
        phase: Some(phase), from_ms: t_ms.saturating_sub(millis(window.to.saturating_sub(window.from))), to_ms: Some(t_ms),
        message: "Latency from planned arrival, including generator delay; raw latency remains in the summary. Skipped arrivals have no synthetic samples.".into(),
        detail: Some(serde_json::json!({
            "method": "scheduled_arrival", "synthetic_samples": 0, "includes_skipped_arrivals": false,
            "chains": identity.chains, "timeline_phase": window.phase,
            "chain_duration": window.metrics.corrected_chain.snapshot(),
            "request_total": window.metrics.corrected_total.snapshot(),
            "ttfb": window.metrics.corrected_ttfb.snapshot(),
            "overflow": {
                "chain_duration": window.metrics.corrected_chain.overflow,
                "request_total": window.metrics.corrected_total.overflow,
                "ttfb": window.metrics.corrected_ttfb.overflow,
            },
        })),
    }))?;
    write_record(writer, buffer, &Record::Annotation(Annotation {
        t_ms, target_id: Some(identity.target.clone()), code: "generator_self_metrics".into(), severity: Severity::Info,
        phase: Some(phase), from_ms: t_ms.saturating_sub(millis(window.to.saturating_sub(window.from))), to_ms: Some(t_ms),
        message: "Sample counts for this interval's maximum send drift and summary timer wake lateness; zero samples means no observation.".into(),
        detail: Some(serde_json::json!({"drift_samples": window.metrics.drift.count(), "drift_overflow": window.metrics.drift.overflow, "scheduler_lag_samples": window.scheduler_lag_samples, "timeline_phase": window.phase})),
    }))?;
    write_record(
        writer,
        buffer,
        &summary_record(identity, t_ms, phase, window, dropped),
    )
}

fn write_record(writer: &mut dyn Write, buffer: &mut Vec<u8>, record: &Record) -> io::Result<()> {
    buffer.clear();
    serde_json::to_writer(&mut *buffer, record).map_err(io::Error::other)?;
    buffer.push(b'\n');
    writer.write_all(buffer)?;
    // Flush each record for live readers, always on this dedicated thread.
    writer.flush()
}

fn summary_record(
    identity: &Identity,
    t_ms: u64,
    phase: Phase,
    window: &Window,
    dropped: u64,
) -> Record {
    let metrics = &window.metrics;
    let counts = &metrics.counters;
    // Straight from what was measured, per chain and per step, rather than from the
    // run's pooled counters: two steps of one chain are two different requests, and
    // a report that pooled them would publish a median of two distributions.
    let chains = metrics
        .chains
        .iter()
        .map(|(name, chain)| {
            let steps = chain
                .steps
                .iter()
                .map(|(id, step)| {
                    (
                        (*id).to_owned(),
                        StepStats {
                            attempted: step.attempted,
                            completed: step.completed,
                            failed: step.failed,
                            statuses: step
                                .statuses
                                .iter()
                                .map(|(status, count)| (status.to_string(), *count))
                                .collect(),
                            errors: error_counts(&step.errors),
                            assertion_failures: step
                                .assertion_failures
                                .iter()
                                .map(|(index, count)| (index.to_string(), *count))
                                .collect(),
                            total: step.total.snapshot(),
                            ttfb: step.ttfb.snapshot(),
                        },
                    )
                })
                .collect();
            (
                (*name).to_owned(),
                ChainStats {
                    iterations_started: chain.started,
                    iterations_completed: chain.completed,
                    iterations_aborted: chain.aborted,
                    duration: chain.duration.snapshot(),
                    steps,
                },
            )
        })
        .collect();
    let seconds = window.to.saturating_sub(window.from).as_secs_f64();
    Record::Summary(Summary {
        t_ms,
        target_id: identity.target.clone(),
        phase,
        window_ms: millis(window.to.saturating_sub(window.from)),
        target_rate: if matches!(phase, Phase::Measure | Phase::Warmup) && window.phase == phase {
            identity.rate
        } else {
            0.0
        },
        achieved_rate: if seconds > 0.0 {
            counts.started as f64 / seconds
        } else {
            0.0
        },
        in_flight: window.in_flight as u32,
        queue_depth: window.queue_depth as u32,
        drift_ms: metrics.drift.max_us().unwrap_or(0) as f64 / 1000.0,
        bytes_sent: counts.bytes_sent,
        bytes_received: counts.bytes_received,
        connections_opened: counts.connections_opened,
        connections_reused: counts.connections_reused,
        chains,
        generator: GeneratorHealth {
            cpu_pct: 0.0,
            rss_bytes: 0,
            open_fds: 0,
            scheduler_lag_ms: window.scheduler_lag.as_secs_f64() * 1000.0,
            headroom_ratio: identity.headroom_ratio,
            events_dropped: dropped,
            // The generator's own cost, beside the rest of its health rather than in
            // the step's latency: §7.3 says a slow generator has to be visible as a
            // slow generator.
            generation: metrics
                .generation
                .iter()
                .map(|(name, counts)| {
                    (
                        (*name).to_owned(),
                        metrix_metrics::events::GenerationStats {
                            calls: counts.calls,
                            failed: counts.failed,
                            duration: counts.duration.snapshot(),
                        },
                    )
                })
                .collect(),
            auth: window.auth.map(|counts| metrix_metrics::events::AuthStats {
                identities: counts.identities,
                acquisitions: counts.acquisitions,
                refreshes: counts.refreshes,
                failures: counts.failures,
                unauthorized: counts.unauthorized,
                refresh_ms: counts.refresh_us as f64 / 1000.0,
                blocked_ms: counts.blocked_us as f64 / 1000.0,
            }),
        },
    })
}

/// The error counter array as the names the frozen schema uses.
fn error_counts(errors: &[u64; 11]) -> BTreeMap<String, u64> {
    // Indexed by `Cause`. Several map to `other` because the transport cannot always
    // tell them apart, and inventing a distinction it did not observe would be worse
    // than saying so.
    const NAMES: [&str; 11] = [
        "dns_failure",
        "other",
        "tls_failure",
        "other",
        "other",
        "other",
        "other",
        "extraction",
        "assertion",
        "generation",
        "unauthorized",
    ];
    let mut counted: BTreeMap<String, u64> = BTreeMap::new();
    for (index, count) in errors.iter().enumerate() {
        if *count > 0 {
            *counted.entry(NAMES[index].to_owned()).or_default() += count;
        }
    }
    counted
}

fn error_class(failure: Failure) -> ErrorClass {
    match failure {
        Failure::Dns => ErrorClass::DnsFailure,
        Failure::Tls => ErrorClass::TlsFailure,
        _ => ErrorClass::Other,
    }
}

fn error_message(failure: Failure) -> &'static str {
    match failure {
        Failure::Dns => "DNS resolution failed",
        Failure::Connect => "connection failed",
        Failure::LocalResource => "local socket resources exhausted",
        Failure::Tls => "TLS failed",
        Failure::Protocol => "HTTP protocol failed",
        Failure::Send => "request send failed",
        Failure::Body => "response body failed",
        Failure::Timeout => "request deadline exceeded",
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}
fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}
