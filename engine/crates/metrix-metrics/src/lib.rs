//! HDR histograms, counters, snapshots, NDJSON output.
//!
//! [`events`] is frozen first: it is the contract the API reads (F0.3). The
//! [`aggregation`] records exclusive worker partitions and merges interval snapshots (B1.3).

pub mod aggregation;
pub mod events;
pub mod stats;

pub use events::{EVENTS_VERSION, HISTOGRAM_ENCODING, Phase, Record, Severity};
