//! Exclusive worker-owned recording; allocation and encoding happen at snapshot time.

use crate::events;
use base64::{Engine, engine::general_purpose::STANDARD};
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use std::time::Duration;

pub const MAX_LATENCY_US: u64 = 3_600_000_000;

#[derive(Clone, Debug)]
pub struct Distribution {
    hdr: Histogram<u64>,
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
    pub errors: [u64; 7],
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
            errors: [0; 7],
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

pub struct Sample {
    pub chain_duration: Duration,
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

/// One chain/step in B1.3. Names and additional chains are added in B3.
#[derive(Clone, Debug, Default)]
pub struct Accumulator {
    pub counters: Counters,
    pub chain: Distribution,
    pub total: Distribution,
    pub ttfb: Distribution,
    pub drift: Distribution,
}

impl Accumulator {
    pub fn start(&mut self) {
        self.counters.started += 1;
    }
    pub fn cancel(&mut self) {
        self.counters.cancelled += 1;
    }

    pub fn finish(&mut self, sample: Sample) {
        if let Some(error) = sample.error {
            self.counters.failed += 1;
            self.counters.errors[error as usize] += 1;
        } else {
            self.counters.completed += 1;
        }
        self.chain.record(sample.chain_duration);
        if let Some(total) = sample.request_duration {
            self.counters.sent_finished += 1;
            self.total.record(total);
        }
        if let Some(ttfb) = sample.ttfb {
            self.ttfb.record(ttfb);
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
        self.chain.merge(&other.chain);
        self.total.merge(&other.total);
        self.ttfb.merge(&other.ttfb);
        self.drift.merge(&other.drift);
    }

    pub fn reset(&mut self) {
        self.counters = Counters::default();
        self.chain.reset();
        self.total.reset();
        self.ttfb.reset();
        self.drift.reset();
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
