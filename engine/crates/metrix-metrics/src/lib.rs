//! HDR histograms, counters, snapshots, NDJSON output.
//!
//! [`events`] is frozen first: it is the contract the API reads (F0.3). The
//! aggregation that produces those records is track B (B1.3).

pub mod events;

pub use events::{EVENTS_VERSION, HISTOGRAM_ENCODING, Phase, Record, Severity};
