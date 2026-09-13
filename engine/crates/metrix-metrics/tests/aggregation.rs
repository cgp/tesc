use base64::{Engine, engine::general_purpose::STANDARD};
use hdrhistogram::{Histogram, serialization::Deserializer};
use metrix_metrics::aggregation::{Accumulator, Cause, Distribution, MAX_LATENCY_US, Sample};
use std::{io::Cursor, time::Duration};

fn sample(us: u64) -> Sample {
    Sample {
        chain_duration: Duration::from_micros(us + 100),
        request_duration: Some(Duration::from_micros(us)),
        ttfb: Some(Duration::from_micros(us / 2)),
        drift: Some(Duration::from_micros(10)),
        status: Some(200),
        error: None,
        bytes_sent: 12,
        bytes_received: 24,
        connections_opened: 0,
        connection_reused: true,
    }
}

#[test]
fn uneven_worker_populations_merge_by_samples_and_survive_v2_serialization() {
    let mut workers = vec![Accumulator::default(), Accumulator::default()];
    for _ in 0..99 {
        workers[0].start();
        workers[0].finish(sample(1000));
    }
    workers[1].start();
    workers[1].finish(sample(100_000));
    let mut merged = Accumulator::default();
    merged.merge_and_reset(&mut workers);
    assert_eq!(merged.counters.started, 100);
    assert_eq!(merged.counters.completed, 100);
    assert_eq!(merged.counters.statuses[200], 100);
    assert_eq!(merged.counters.bytes_sent, 1200);
    assert_eq!(merged.counters.bytes_received, 2400);
    let snapshot = merged.total.snapshot();
    assert_eq!(snapshot.count, 100);
    assert_eq!(snapshot.min_us, Some(1000));
    assert_eq!(snapshot.max_us, Some(100_000));
    assert_eq!(snapshot.mean_us, Some(1990.0)); // averaging the worker means would be wrong.
    let encoded = STANDARD.decode(snapshot.hdr.unwrap()).unwrap();
    let decoded: Histogram<u64> = Deserializer::new()
        .deserialize(&mut Cursor::new(encoded))
        .unwrap();
    assert_eq!(decoded.len(), 100);
    assert_eq!(decoded.count_at(1000), 99);
    assert_eq!(decoded.count_at(100_000), 1);
    assert_eq!(
        decoded.value_at_quantile(0.95),
        decoded.highest_equivalent(1000)
    );
    assert!(!decoded.is_auto_resize());
    assert!(
        workers
            .iter()
            .all(|w| w.counters.started == 0 && w.total.count() == 0)
    );
    let mut cumulative = Accumulator::default();
    cumulative.merge(&merged);
    workers[1].start();
    workers[1].finish(sample(2000));
    merged.merge_and_reset(&mut workers);
    assert_eq!(merged.total.count(), 1);
    cumulative.merge(&merged);
    assert_eq!(cumulative.total.count(), 101);
    assert_eq!(cumulative.counters.statuses[200], 101);
}

#[test]
fn empty_zero_and_overflow_distributions_are_explicit_and_resettable() {
    let mut distribution = Distribution::default();
    let empty = distribution.snapshot();
    assert_eq!(empty.count, 0);
    assert!(
        empty.hdr.is_none()
            && empty.min_us.is_none()
            && empty.max_us.is_none()
            && empty.mean_us.is_none()
    );
    distribution.record(Duration::from_nanos(999));
    distribution.record(Duration::from_micros(MAX_LATENCY_US));
    distribution.record(Duration::from_micros(MAX_LATENCY_US + 1));
    distribution.record(Duration::MAX);
    let snapshot = distribution.snapshot();
    assert_eq!(snapshot.count, 2);
    assert_eq!(snapshot.min_us, Some(0));
    assert_eq!(snapshot.max_us, Some(MAX_LATENCY_US));
    assert_eq!(distribution.overflow, 2);
    let mut merged = Distribution::default();
    merged.merge(&distribution);
    assert_eq!(merged.overflow, 2);
    assert_eq!(merged.count(), 2);
    distribution.reset();
    assert_eq!(distribution.overflow, 0);
    assert_eq!(distribution.snapshot(), empty);
}

#[test]
fn failures_before_send_body_failures_and_cancellation_have_distinct_sample_counts() {
    let mut worker = Accumulator::default();
    worker.start();
    let mut connect_failure = sample(10);
    connect_failure.request_duration = None;
    connect_failure.ttfb = None;
    connect_failure.drift = None;
    connect_failure.status = None;
    connect_failure.error = Some(Cause::Connect);
    connect_failure.bytes_sent = 0;
    connect_failure.bytes_received = 0;
    connect_failure.connection_reused = false;
    worker.finish(connect_failure);
    worker.start();
    let mut body_failure = sample(50);
    body_failure.status = Some(503);
    body_failure.error = Some(Cause::Body);
    worker.finish(body_failure);
    worker.start();
    worker.cancel();
    assert_eq!(worker.counters.started, 3);
    assert_eq!(worker.counters.failed, 2);
    assert_eq!(worker.counters.completed, 0);
    assert_eq!(worker.counters.cancelled, 1);
    assert_eq!(worker.counters.errors[Cause::Connect as usize], 1);
    assert_eq!(worker.counters.errors[Cause::Body as usize], 1);
    assert_eq!(worker.counters.statuses[503], 1);
    assert_eq!(worker.chain.count(), 2);
    assert_eq!(worker.total.count(), 1);
    assert_eq!(worker.ttfb.count(), 1);
    assert_eq!(worker.drift.count(), 1);
}
