//! Bounded output queues. Only writer threads construct and serialize request records.

use crate::{Failure, Plan, http::Observation};
use chrono::{SecondsFormat, Utc};
use metrix_metrics::{Record, aggregation::Window, events::*};
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

const RESERVED: usize = 8;
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

struct Identity {
    target: String,
    chain: String,
    step: String,
    call: String,
    rate: f64,
}

struct RequestData {
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
    Record(Box<Record>),
    Summary {
        t_ms: u64,
        phase: Phase,
        window: Box<Window>,
    },
    Request(RequestData),
}

impl Packet {
    fn time(&self) -> u64 {
        match self {
            Self::Fence(_) => unreachable!("fences do not produce records"),
            Self::Summary { t_ms, .. } => *t_ms,
            Self::Request(request) => request.t_ms,
            Self::Record(record) => match record.as_ref() {
                Record::RunStarted(r) => r.t_ms,
                Record::PhaseChanged(r) => r.t_ms,
                Record::TargetStarted(r) => r.t_ms,
                Record::TargetFinished(r) => r.t_ms,
                Record::Annotation(r) => r.t_ms,
                Record::RunFinished(r) => r.t_ms,
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
    summary: Stream,
    events: Option<Stream>,
    identity: Arc<Identity>,
    losses: Arc<Losses>,
    start: Instant,
    sample_rate: f64,
    seed: u64,
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
            chain: plan.chain.clone(),
            step: plan.step.clone(),
            call: plan.call.clone(),
            rate: plan.rate,
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
            summary,
            events,
            identity,
            losses,
            start,
            sample_rate,
            seed,
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
            machine_profile: None,
            targets: vec![plan.target.id.clone()],
            histogram_encoding: HISTOGRAM_ENCODING.into(),
        }));
        output.lifecycle(Record::TargetStarted(TargetStarted {
            t_ms: 0,
            target_id: plan.target.id.clone(),
            index: 1,
            total: 1,
        }));
        output.lifecycle(Record::Annotation(Annotation {
            t_ms: 0, target_id: None, code: "self_metrics_unavailable".into(), severity: Severity::Info,
            phase: None, from_ms: 0, to_ms: None,
            message: "Zero in cpu_pct, rss_bytes, open_fds, scheduler_lag_ms and queue_depth means unavailable; self-metric collectors are not implemented yet.".into(),
            detail: Some(serde_json::json!({"unavailable": ["cpu_pct", "rss_bytes", "open_fds", "scheduler_lag_ms", "queue_depth"]})),
        }));
        Ok(output)
    }

    pub(crate) fn elapsed(&self) -> u64 {
        millis(self.start.elapsed())
    }

    fn lifecycle(&self, record: Record) {
        // Reserved slots cover all lifecycle records even if the writer never reads.
        for stream in std::iter::once(&self.summary).chain(self.events.iter()) {
            if stream
                .sender
                .try_send(Packet::Record(Box::new(record.clone())))
                .is_err()
            {
                self.losses.failed.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn phase(&self, phase: Phase) {
        self.lifecycle(Record::PhaseChanged(PhaseChanged {
            t_ms: self.elapsed(),
            target_id: self.identity.target.clone(),
            phase,
        }));
    }

    pub(crate) fn summary(&self, window: &Window, phase: Phase) {
        if self.summary.sender.capacity() <= RESERVED
            || self
                .summary
                .sender
                .try_send(Packet::Summary {
                    t_ms: self.elapsed(),
                    phase,
                    window: Box::new(window.clone()),
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
        observation: &Observation,
        body_len: usize,
    ) {
        self.request_data(RequestData {
            t_ms: self.elapsed(),
            phase,
            iteration,
            ttfb_us: observation.ttfb.map(micros),
            total_us: micros(observation.request_duration.unwrap_or(observation.total)),
            status: observation.status,
            bytes_sent: if observation.sent.is_some() {
                body_len as u64
            } else {
                0
            },
            bytes_received: observation.bytes_received,
            connection_reused: observation.connection_reused,
            error: observation.error,
            cancelled: false,
        });
    }

    pub(crate) fn cancel(&self, iteration: u64, phase: Phase, elapsed: Duration) {
        self.request_data(RequestData {
            t_ms: self.elapsed(),
            phase,
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

    /// Wait once with a shared deadline. Stalled writers are detached, never joined.
    pub fn finish(self, mut exit_code: i32, stopped_because: Option<String>) -> OutputReport {
        let target_completed = exit_code == 0;
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
        if self.losses.failed.load(Ordering::Relaxed) && exit_code == 0 {
            exit_code = 1;
        }
        let t_ms = self.elapsed();
        self.lifecycle(Record::TargetFinished(TargetFinished {
            t_ms,
            target_id: self.identity.target.clone(),
            completed: target_completed,
        }));
        self.lifecycle(Record::RunFinished(RunFinished {
            t_ms,
            exit_code,
            slo: vec![],
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
            let result = write_stream(writer, receiver, &identity, &losses, sampled);
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
    identity: &Identity,
    losses: &Losses,
    sampled: bool,
) -> io::Result<()> {
    let mut buffer = Vec::with_capacity(4096);
    let mut last_losses = (0, 0, 0);
    let mut started = false;
    while let Some(packet) = receiver.blocking_recv() {
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
            Packet::Record(record) => *record,
            Packet::Summary {
                t_ms,
                phase,
                window,
            } => summary_record(identity, t_ms, phase, &window, current.1),
            Packet::Request(data) => Record::Request(RequestEvent {
                t_ms: data.t_ms,
                target_id: identity.target.clone(),
                phase: data.phase,
                chain: identity.chain.clone(),
                iteration: data.iteration,
                step: identity.step.clone(),
                call: identity.call.clone(),
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
    let errors = counts
        .errors
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .fold(BTreeMap::new(), |mut errors, (index, count)| {
            let name = [
                "dns_failure",
                "other",
                "tls_failure",
                "other",
                "other",
                "other",
                "other",
            ][index];
            *errors.entry(name.into()).or_default() += count;
            errors
        });
    let step = StepStats {
        attempted: counts.started,
        completed: counts.completed,
        failed: counts.failed,
        statuses: counts
            .statuses
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(status, count)| (status.to_string(), *count))
            .collect(),
        errors,
        assertion_failures: BTreeMap::new(),
        total: metrics.total.snapshot(),
        ttfb: metrics.ttfb.snapshot(),
    };
    let chain = ChainStats {
        iterations_started: counts.started,
        iterations_completed: counts.completed,
        iterations_aborted: counts.failed + counts.cancelled,
        duration: metrics.chain.snapshot(),
        steps: BTreeMap::from([(identity.step.clone(), step)]),
    };
    let seconds = window.to.saturating_sub(window.from).as_secs_f64();
    Record::Summary(Summary {
        t_ms,
        target_id: identity.target.clone(),
        phase,
        window_ms: millis(window.to.saturating_sub(window.from)),
        target_rate: if phase == Phase::Measure {
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
        queue_depth: 0,
        drift_ms: metrics.drift.snapshot().max_us.unwrap_or(0) as f64 / 1000.0,
        bytes_sent: counts.bytes_sent,
        bytes_received: counts.bytes_received,
        connections_opened: counts.connections_opened,
        connections_reused: counts.connections_reused,
        chains: BTreeMap::from([(identity.chain.clone(), chain)]),
        generator: GeneratorHealth {
            cpu_pct: 0.0,
            rss_bytes: 0,
            open_fds: 0,
            scheduler_lag_ms: 0.0,
            headroom_ratio: None,
            events_dropped: dropped,
        },
    })
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
