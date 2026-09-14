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
        self.overflow += other.overflow;
        if other.count() == 0 {
            return;
        }
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
    }

    pub fn reset(&mut self) {
        if self.count() > 0 {
            self.hdr.reset();
        }
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
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
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
    /// The request happened and the answer was not the one the plan expects. Apart
    /// from the transport failures because it is a different fact about the run.
    Assertion,
    /// A generator could not build the request, so none was sent. Ours, not theirs
    /// (§7.3): a generation failure counted as a target error would report a working
    /// service as broken by the plan's own script.
    Generation,
    /// The credential was rejected or could not be obtained. Apart from application
    /// errors (§6.1) because it is a different thing to go and fix, and because a 401
    /// storm reads very differently from a service returning 500s.
    Unauthorized,
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
    pub errors: [u64; 11],
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
            errors: [0; 11],
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
    /// Which assertion the response failed, if one did. Travels beside `error`
    /// rather than inside it: the request succeeded and the answer was wrong, and
    /// the two are different facts about the run.
    pub assertion: Option<usize>,
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
    pub errors: [u64; 11],
    /// Which assertion failed, by its index in the call. Named by index because that
    /// is what the call document is indexed by, and a message would be a second
    /// place for the assertion's meaning to live.
    pub assertion_failures: BTreeMap<usize, u64>,
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
        for (index, count) in &other.assertion_failures {
            *self.assertion_failures.entry(*index).or_default() += count;
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
        self.assertion_failures.clear();
        self.errors = [0; 11];
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
    /// What each generator cost and how often it failed (§7.3). Apart from request
    /// latency on purpose: generation is the generator's time, not the service's, and
    /// folding it in would make a slow script look like a slow endpoint.
    pub generation: BTreeMap<&'static str, GenerationCounts>,
}

/// One generator's own cost over a window.
#[derive(Clone, Debug, Default)]
pub struct GenerationCounts {
    pub calls: u64,
    pub failed: u64,
    pub duration: Distribution,
}

impl GenerationCounts {
    fn merge(&mut self, other: &Self) {
        self.calls += other.calls;
        self.failed += other.failed;
        self.duration.merge(&other.duration);
    }

    fn reset(&mut self) {
        self.calls = 0;
        self.failed = 0;
        self.duration.reset();
    }
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

    /// Name a generator before it runs, for the same reason chains are named: a
    /// window in which a generator was not called is a fact about the window.
    pub fn declare_generator(&mut self, generator: &'static str) {
        self.generation.entry(generator).or_default();
    }

    /// One call of one generator, whether or not it produced a request.
    pub fn finish_generation(&mut self, generator: &'static str, took: Duration, failed: bool) {
        let counts = self.generation.entry(generator).or_default();
        counts.calls += 1;
        counts.failed += u64::from(failed);
        counts.duration.record(took);
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
        if let Some(index) = sample.assertion {
            *step.assertion_failures.entry(index).or_default() += 1;
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
        for (name, counts) in &other.generation {
            self.generation.entry(name).or_default().merge(counts);
        }
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
        for counts in self.generation.values_mut() {
            counts.reset();
        }
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

/// Arrival-clock timing, separate from requests and the 250ms summary timer.
#[derive(Clone, Debug, Default)]
pub struct ArrivalTiming {
    pub timer_wake_lateness: Distribution,
    pub wake_to_dispatch: Distribution,
    pub timer_wakes: u64,
    pub coalesced_wakes: u64,
    pub telemetry_dropped: u64,
    pub timer_skipped_arrivals: u64,
    pub dispatches: u64,
    pub dispatches_with_skips: u64,
    pub skipped_arrivals: u64,
    pub max_skipped_per_dispatch: u64,
    /// Exact skip counts 0..30; bucket 31 contains 31 or more.
    pub skipped_per_dispatch: [u64; 32],
    pub phase_end_skipped: u64,
}

impl ArrivalTiming {
    pub fn dispatch(&mut self, skipped: u64, phase_end: bool) {
        if phase_end {
            self.phase_end_skipped += skipped;
        } else {
            self.dispatches += 1;
            self.skipped_per_dispatch[skipped.min(31) as usize] += 1;
            self.dispatches_with_skips += u64::from(skipped > 0);
            self.max_skipped_per_dispatch = self.max_skipped_per_dispatch.max(skipped);
        }
        self.skipped_arrivals += skipped;
    }
    pub fn merge(&mut self, other: &Self) {
        self.timer_wake_lateness.merge(&other.timer_wake_lateness);
        self.wake_to_dispatch.merge(&other.wake_to_dispatch);
        self.timer_wakes += other.timer_wakes;
        self.coalesced_wakes += other.coalesced_wakes;
        self.telemetry_dropped += other.telemetry_dropped;
        self.timer_skipped_arrivals += other.timer_skipped_arrivals;
        self.dispatches += other.dispatches;
        self.dispatches_with_skips += other.dispatches_with_skips;
        self.skipped_arrivals += other.skipped_arrivals;
        self.max_skipped_per_dispatch = self
            .max_skipped_per_dispatch
            .max(other.max_skipped_per_dispatch);
        self.phase_end_skipped += other.phase_end_skipped;
        for (count, other) in self
            .skipped_per_dispatch
            .iter_mut()
            .zip(other.skipped_per_dispatch)
        {
            *count += other;
        }
    }
    pub fn reset(&mut self) {
        self.timer_wake_lateness.reset();
        self.wake_to_dispatch.reset();
        self.timer_wakes = 0;
        self.coalesced_wakes = 0;
        self.telemetry_dropped = 0;
        self.timer_skipped_arrivals = 0;
        self.dispatches = 0;
        self.dispatches_with_skips = 0;
        self.skipped_arrivals = 0;
        self.max_skipped_per_dispatch = 0;
        self.phase_end_skipped = 0;
        self.skipped_per_dispatch.fill(0);
    }
}

/// Completed stages of one snapshot; total is populated after consumer handoff.
#[derive(Clone, Copy, Debug, Default)]
pub struct SnapshotSample {
    pub aggregation: Duration,
    pub window_construction: Duration,
    pub flush_total: Option<Duration>,
    pub output_packet: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotTiming {
    pub paired: Distribution,
    pub aggregation: Distribution,
    pub window_construction: Distribution,
    pub flush_total: Distribution,
    pub output_packet: Distribution,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub snapshot: SnapshotSample,
    pub arrival: Option<Box<ArrivalTiming>>,
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
    /// What auth cost over the run so far, when the plan has an `auth` block. Whole-
    /// run totals rather than per-window deltas: acquisitions and refreshes are rare
    /// enough that a window's worth of them is usually zero, and the question a
    /// reader has is how much of this run was spent on tokens.
    pub auth: Option<AuthCounts>,
}

/// What getting and keeping credentials has cost (§6.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthCounts {
    pub identities: u64,
    pub acquisitions: u64,
    pub refreshes: u64,
    pub failures: u64,
    /// 401s from the target that caused a refresh.
    pub unauthorized: u64,
    pub refresh_us: u64,
    /// Virtual-user time spent waiting on somebody else's refresh.
    pub blocked_us: u64,
}

#[cfg(test)]
mod arrival_tests {
    use super::*;
    #[test]
    fn interval_merges_preserve_samples_skips_and_maxima_then_reset() {
        let mut first = ArrivalTiming::default();
        first.timer_wake_lateness.record(Duration::from_micros(2));
        first.wake_to_dispatch.record(Duration::from_millis(20));
        first.timer_wakes = 1;
        first.dispatch(4, false);
        let mut second = ArrivalTiming::default();
        second.timer_wake_lateness.record(Duration::from_micros(10));
        second.wake_to_dispatch.record(Duration::from_millis(1));
        second.timer_wakes = 3;
        second.telemetry_dropped = 2;
        second.coalesced_wakes = 2;
        second.dispatch(1, false);
        second.dispatch(2, true);
        first.merge(&second);
        assert_eq!(first.timer_wakes, 4);
        assert_eq!(first.telemetry_dropped, 2);
        assert_eq!(first.timer_wake_lateness.count(), 2);
        assert_eq!(first.timer_wake_lateness.max_us(), Some(10));
        assert_eq!(first.wake_to_dispatch.max_us(), Some(20_000));
        assert_eq!(first.dispatches_with_skips, 2);
        assert_eq!(first.max_skipped_per_dispatch, 4);
        assert_eq!(first.skipped_arrivals, 7);
        assert_eq!(first.phase_end_skipped, 2);
        assert_eq!(first.skipped_per_dispatch[4], 1);
        assert_eq!(first.skipped_per_dispatch[1], 1);
        assert_eq!(
            first.skipped_per_dispatch.iter().sum::<u64>(),
            first.dispatches
        );
        first.reset();
        assert_eq!(first.timer_wake_lateness.count(), 0);
        assert_eq!(first.skipped_per_dispatch.iter().sum::<u64>(), 0);
        assert_eq!(
            first.timer_wakes
                + first.telemetry_dropped
                + first.skipped_arrivals
                + first.max_skipped_per_dispatch
                + first.coalesced_wakes,
            0
        );
    }
}
