//! Exclusive worker-owned recording; allocation and encoding happen at snapshot time.

use crate::events;
use base64::{Engine, engine::general_purpose::STANDARD};
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use std::{collections::BTreeMap, time::Duration};

pub const MAX_LATENCY_US: u64 = 3_600_000_000;

#[derive(Clone, Debug)]
pub struct Distribution {
    pub(crate) hdr: Histogram<u64>,
    min: Option<u64>,
    max: Option<u64>,
    sum: u128,
    pub overflow: u64,
}

impl Default for Distribution {
    fn default() -> Self {
        Self {
            hdr: Histogram::new_with_bounds(1, MAX_LATENCY_US, 3).expect("fixed HDR configuration"),
            min: None,
            max: None,
            sum: 0,
            overflow: 0,
        }
    }
}

impl Distribution {
    pub fn record(&mut self, duration: Duration) {
        let us = duration.as_micros();
        if us > u128::from(MAX_LATENCY_US) {
            self.overflow += 1;
            return;
        }
        let us = us as u64;
        self.hdr
            .record(us)
            .expect("sample within preallocated histogram");
        self.min = Some(self.min.map_or(us, |min| min.min(us)));
        self.max = Some(self.max.map_or(us, |max| max.max(us)));
        self.sum += u128::from(us);
    }

    pub fn count(&self) -> u64 {
        self.hdr.len()
    }

    /// Exact observed maximum without allocating or encoding the histogram.
    pub fn max_us(&self) -> Option<u64> {
        self.max
    }

    pub fn merge(&mut self, other: &Self) {
        self.hdr
            .add(&other.hdr)
            .expect("identically configured histograms");
        if let Some(min) = other.min {
            self.min = Some(self.min.map_or(min, |old| old.min(min)));
        }
        if let Some(max) = other.max {
            self.max = Some(self.max.map_or(max, |old| old.max(max)));
        }
        self.sum += other.sum;
        self.overflow += other.overflow;
    }

    pub fn reset(&mut self) {
        self.hdr.reset();
        self.min = None;
        self.max = None;
        self.sum = 0;
        self.overflow = 0;
    }

    /// Exact summaries plus the mergeable distribution; no percentile support claim.
    pub fn snapshot(&self) -> events::Histogram {
        let hdr = if self.count() == 0 {
            None
        } else {
            let mut buffer = Vec::new();
            V2Serializer::new()
                .serialize(&self.hdr, &mut buffer)
                .expect("encoding bounded HDR into memory");
            Some(STANDARD.encode(buffer))
        };
        events::Histogram {
            count: self.count(),
            min_us: self.min,
            max_us: self.max,
            mean_us: (self.count() > 0).then(|| self.sum as f64 / self.count() as f64),
            hdr,
        }
    }
}

/// Fixed-index causes keep error recording free of strings and map insertions.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Cause {
    Dns,
    Connect,
    Tls,
    Protocol,
    Send,
    Body,
    Timeout,
    /// A value a later step needed was never captured, so the request could not be
    /// built. Ours rather than theirs: nothing was sent, and counting it as a
    /// transport failure would point at the service.
    Extraction,
}

#[derive(Clone, Debug)]
pub struct Counters {
    pub started: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub sent_finished: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connections_opened: u64,
    pub connections_reused: u64,
    pub statuses: [u64; 1000],
    pub errors: [u64; 8],
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            started: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            sent_finished: 0,
            bytes_sent: 0,
            bytes_received: 0,
            connections_opened: 0,
            connections_reused: 0,
            statuses: [0; 1000],
            errors: [0; 8],
        }
    }
}

impl Counters {
    fn merge(&mut self, other: &Self) {
        self.started += other.started;
        self.completed += other.completed;
        self.failed += other.failed;
        self.cancelled += other.cancelled;
        self.sent_finished += other.sent_finished;
        self.bytes_sent += other.bytes_sent;
        self.bytes_received += other.bytes_received;
        self.connections_opened += other.connections_opened;
        self.connections_reused += other.connections_reused;
        for (left, right) in self.statuses.iter_mut().zip(other.statuses) {
            *left += right;
        }
        for (left, right) in self.errors.iter_mut().zip(other.errors) {
            *left += right;
        }
    }
}

/// One step's contribution: what it was, and what came back.
pub struct StepSample {
    /// Which chain and which step within it. Both are names from the plan, because
    /// they are what every chart series, error report and SLO is keyed by.
    pub chain: &'static str,
    pub step: &'static str,
    pub request_duration: Option<Duration>,
    pub send_delay: Duration,
    pub ttfb: Option<Duration>,
    pub drift: Option<Duration>,
    pub status: Option<u16>,
    pub error: Option<Cause>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connections_opened: u64,
    pub connection_reused: bool,
}

pub struct Sample {
    pub chain_duration: Duration,
    pub admission_delay: Duration,
    pub send_delay: Duration,
    pub request_duration: Option<Duration>,
    pub ttfb: Option<Duration>,
    pub drift: Option<Duration>,
    pub status: Option<u16>,
    pub error: Option<Cause>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connections_opened: u64,
    pub connection_reused: bool,
}

/// One step of one chain, measured on its own.
///
/// Per-step numbers find the slow endpoint; the chain's own duration is what a user
/// feels (design-engine §5). Both are kept, and neither is derived from the other:
/// a chain's duration is not the sum of its steps' medians, and a report that added
/// them would be inventing a figure nobody measured.
#[derive(Clone, Debug, Default)]
pub struct StepStats {
    pub attempted: u64,
    pub completed: u64,
    pub failed: u64,
    pub statuses: BTreeMap<u16, u64>,
    pub errors: [u64; 8],
    pub total: Distribution,
    pub ttfb: Distribution,
}

impl StepStats {
    fn merge(&mut self, other: &Self) {
        self.attempted += other.attempted;
        self.completed += other.completed;
        self.failed += other.failed;
        for (status, count) in &other.statuses {
            *self.statuses.entry(*status).or_default() += count;
        }
        for (left, right) in self.errors.iter_mut().zip(other.errors) {
            *left += right;
        }
        self.total.merge(&other.total);
        self.ttfb.merge(&other.ttfb);
    }

    fn reset(&mut self) {
        self.attempted = 0;
        self.completed = 0;
        self.failed = 0;
        // Cleared rather than dropped: the same statuses recur every window, and a
        // map that is emptied and refilled once a second allocates for nothing.
        self.statuses.clear();
        self.errors = [0; 8];
        self.total.reset();
        self.ttfb.reset();
    }
}

/// One chain, measured end to end, with its steps inside it.
#[derive(Clone, Debug, Default)]
pub struct ChainStats {
    pub started: u64,
    pub completed: u64,
    pub aborted: u64,
    pub duration: Distribution,
    pub steps: BTreeMap<&'static str, StepStats>,
}

impl ChainStats {
    fn merge(&mut self, other: &Self) {
        self.started += other.started;
        self.completed += other.completed;
        self.aborted += other.aborted;
        self.duration.merge(&other.duration);
        for (id, step) in &other.steps {
            self.steps.entry(id).or_default().merge(step);
        }
    }

    fn reset(&mut self) {
        self.started = 0;
        self.completed = 0;
        self.aborted = 0;
        self.duration.reset();
        // Keys kept, values cleared: the chain's steps are fixed for the run, so
        // every window after the first finds the histograms already allocated.
        for step in self.steps.values_mut() {
            step.reset();
        }
    }
}

/// Everything one worker measured in one window.
///
/// The pooled distributions and the per-chain ones are both kept: the pooled figures
/// are what a run's own percentiles are computed from, and the per-chain ones are
/// what the summary stream reports. Deriving either from the other would mean either
/// merging percentiles or pooling across chains, and neither is a thing that can be
/// done honestly.
#[derive(Clone, Debug, Default)]
pub struct Accumulator {
    pub counters: Counters,
    pub chain: Distribution,
    pub total: Distribution,
    pub ttfb: Distribution,
    pub drift: Distribution,
    pub corrected_chain: Distribution,
    pub corrected_total: Distribution,
    pub corrected_ttfb: Distribution,
    pub chains: BTreeMap<&'static str, ChainStats>,
}

impl Accumulator {
    /// Name the chains and steps this run has, before any of them runs.
    ///
    /// Every window then reports every chain, including the ones that sent nothing
    /// in it. A zero is a measurement -- this chain did nothing during this window --
    /// and a consumer that had to tell an absent chain from an idle one would be
    /// reconstructing the plan to do it. It also keeps the maps out of the hot path:
    /// the keys exist before the first iteration and are never inserted again.
    pub fn declare(&mut self, chain: &'static str, steps: &[&'static str]) {
        let stats = self.chains.entry(chain).or_default();
        for step in steps {
            stats.steps.entry(step).or_default();
        }
    }

    pub fn start(&mut self) {
        self.counters.started += 1;
    }

    /// One iteration of one chain has been admitted.
    pub fn start_chain(&mut self, chain: &'static str) {
        self.counters.started += 1;
        self.chains.entry(chain).or_default().started += 1;
    }

    pub fn cancel(&mut self) {
        self.counters.cancelled += 1;
    }

    /// One iteration of one chain was cancelled in flight.
    pub fn cancel_chain(&mut self, chain: &'static str) {
        self.counters.cancelled += 1;
        self.chains.entry(chain).or_default().aborted += 1;
    }

    /// One step finished, however it finished.
    pub fn finish_step(&mut self, sample: &StepSample) {
        let step = self
            .chains
            .entry(sample.chain)
            .or_default()
            .steps
            .entry(sample.step)
            .or_default();
        step.attempted += 1;
        if let Some(error) = sample.error {
            step.failed += 1;
            step.errors[error as usize] += 1;
        } else {
            step.completed += 1;
        }
        if let Some(status) = sample.status {
            *step.statuses.entry(status).or_default() += 1;
        }
        if let Some(total) = sample.request_duration {
            step.total.record(total);
            self.counters.sent_finished += 1;
            self.total.record(total);
            self.corrected_total
                .record(total.saturating_add(sample.send_delay));
        }
        if let Some(ttfb) = sample.ttfb {
            step.ttfb.record(ttfb);
            self.ttfb.record(ttfb);
            self.corrected_ttfb
                .record(ttfb.saturating_add(sample.send_delay));
        }
        if let Some(drift) = sample.drift {
            self.drift.record(drift);
        }
        if let Some(status) = sample.status {
            if let Some(counter) = self.counters.statuses.get_mut(usize::from(status)) {
                *counter += 1;
            }
        }
        if let Some(error) = sample.error {
            self.counters.errors[error as usize] += 1;
        }
        self.counters.bytes_sent += sample.bytes_sent;
        self.counters.bytes_received += sample.bytes_received;
        self.counters.connections_opened += sample.connections_opened;
        self.counters.connections_reused += u64::from(sample.connection_reused);
    }

    /// One iteration is over, whether or not it got through every step.
    ///
    /// The duration is recorded either way: an iteration that stopped at its second
    /// step still took the time it took, and dropping those would make the chain's
    /// median a median of the runs that happened to work.
    pub fn finish_chain(
        &mut self,
        chain: &'static str,
        duration: Duration,
        admission_delay: Duration,
        aborted: bool,
    ) {
        let stats = self.chains.entry(chain).or_default();
        stats.duration.record(duration);
        if aborted {
            stats.aborted += 1;
            self.counters.failed += 1;
        } else {
            stats.completed += 1;
            self.counters.completed += 1;
        }
        self.chain.record(duration);
        self.corrected_chain
            .record(duration.saturating_add(admission_delay));
    }

    pub fn finish(&mut self, sample: Sample) {
        if let Some(error) = sample.error {
            self.counters.failed += 1;
            self.counters.errors[error as usize] += 1;
        } else {
            self.counters.completed += 1;
        }
        self.chain.record(sample.chain_duration);
        self.corrected_chain
            .record(sample.chain_duration.saturating_add(sample.admission_delay));
        if let Some(total) = sample.request_duration {
            self.counters.sent_finished += 1;
            self.total.record(total);
            self.corrected_total
                .record(total.saturating_add(sample.send_delay));
        }
        if let Some(ttfb) = sample.ttfb {
            self.ttfb.record(ttfb);
            self.corrected_ttfb
                .record(ttfb.saturating_add(sample.send_delay));
        }
        if let Some(drift) = sample.drift {
            self.drift.record(drift);
        }
        if let Some(status) = sample.status {
            if let Some(counter) = self.counters.statuses.get_mut(usize::from(status)) {
                *counter += 1;
            }
        }
        self.counters.bytes_sent += sample.bytes_sent;
        self.counters.bytes_received += sample.bytes_received;
        self.counters.connections_opened += sample.connections_opened;
        self.counters.connections_reused += u64::from(sample.connection_reused);
    }

    pub fn merge(&mut self, other: &Self) {
        self.counters.merge(&other.counters);
        for (name, chain) in &other.chains {
            self.chains.entry(name).or_default().merge(chain);
        }
        self.chain.merge(&other.chain);
        self.total.merge(&other.total);
        self.ttfb.merge(&other.ttfb);
        self.drift.merge(&other.drift);
        self.corrected_chain.merge(&other.corrected_chain);
        self.corrected_total.merge(&other.corrected_total);
        self.corrected_ttfb.merge(&other.corrected_ttfb);
    }

    pub fn reset(&mut self) {
        self.counters = Counters::default();
        for chain in self.chains.values_mut() {
            chain.reset();
        }
        self.chain.reset();
        self.total.reset();
        self.ttfb.reset();
        self.drift.reset();
        self.corrected_chain.reset();
        self.corrected_total.reset();
        self.corrected_ttfb.reset();
    }

    pub fn merge_and_reset(&mut self, workers: &mut [Self]) {
        self.reset();
        for worker in workers {
            self.merge(worker);
            worker.reset();
        }
    }
}

#[derive(Clone, Debug)]
pub struct Window {
    pub phase: events::Phase,
    pub warmup_metrics: Option<Box<Accumulator>>,
    pub warmup_in_flight: usize,
    pub warmup_queue_depth: usize,
    pub from: Duration,
    pub to: Duration,
    pub in_flight: usize,
    pub queue_depth: usize,
    pub scheduler_lag: Duration,
    pub scheduler_lag_samples: u64,
    pub metrics: Accumulator,
}
