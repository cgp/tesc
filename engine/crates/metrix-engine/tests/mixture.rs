//! B3.3: several chains sharing one rate, in the proportions the mixture declares.

mod support;

use std::{future::pending, net::SocketAddr, path::Path};

use metrix_engine::{Plan, Report, run};
use metrix_mock::{Config, MockServer};
use serde_json::{Value, json};
use support::{bundle, edit};
use tokio::sync::oneshot;

/// Three chains at the given shares, all hitting the mock.
fn mixed(root: &Path, address: SocketAddr, shares: [f64; 3]) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({"ping": {"method": "GET", "path": "/ping"}}),
    );
    edit(root, "mix.json", |doc| {
        doc["chains"] = json!([
            {"name": "a", "percent": shares[0], "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
            {"name": "b", "percent": shares[1], "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
            {"name": "c", "percent": shares[2], "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]}
        ]);
        // Enough iterations for a proportion to mean something, and few enough in
        // flight that the test's own single-threaded runtime is not what is being
        // measured: the mock runs on the same thread as the generator here.
        doc["load"]["rate"] = json!(60);
        doc["load"]["duration"] = json!("2s");
    });
}

async fn run_mixed(shares: [f64; 3]) -> Report {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    mixed(dir.path(), server.local_addr().unwrap(), shares);
    // Bound is not serving: the mock accepts only once it is run.
    let (stop, stopped) = oneshot::channel();
    let serving = tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
    report
}

fn load_with(chains: Value) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "mix.json", |doc| doc["chains"] = chains);
    Plan::load(dir.path()).map(drop)
}

#[tokio::test]
async fn each_chain_gets_the_share_it_asked_for() {
    let report = run_mixed([50.0, 30.0, 20.0]).await;
    let started: u64 = report
        .metrics
        .chains
        .values()
        .map(|chain| chain.started)
        .sum();
    assert!(started > 100, "only {started} iterations ran");

    for (name, share) in [("a", 0.5), ("b", 0.3), ("c", 0.2)] {
        let got = report.metrics.chains[name].started as f64 / started as f64;
        // Within a whisker, not within a confidence interval: the selection is
        // deterministic, so the only slack is the tail of the run.
        assert!(
            (got - share).abs() < 0.02,
            "chain {name} got {got:.3} of the traffic, not {share}"
        );
    }
}

#[tokio::test]
async fn the_shares_hold_at_every_point_and_not_merely_at_the_end() {
    // The counts stay within one of their exact share throughout, which is what
    // keeps a short window representative of the mixture rather than of whichever
    // chain the sampler happened to favour first.
    let report = run_mixed([50.0, 30.0, 20.0]).await;
    let started: u64 = report
        .metrics
        .chains
        .values()
        .map(|chain| chain.started)
        .sum();
    let expected = |share: f64| (started as f64 * share).floor() as u64;
    for (name, share) in [("a", 0.5), ("b", 0.3), ("c", 0.2)] {
        let got = report.metrics.chains[name].started;
        assert!(
            got.abs_diff(expected(share)) <= 1,
            "chain {name}: {got} against {}",
            expected(share)
        );
    }
}

#[tokio::test]
async fn the_same_mixture_runs_the_same_way_twice() {
    let first = run_mixed([50.0, 30.0, 20.0]).await;
    let second = run_mixed([50.0, 30.0, 20.0]).await;
    // Not the same number of iterations -- that depends on the clock -- but the same
    // proportions, because nothing here is sampled.
    for name in ["a", "b", "c"] {
        let ratio =
            |report: &Report| report.metrics.chains[name].started as f64 / report.admitted as f64;
        assert!(
            (ratio(&first) - ratio(&second)).abs() < 0.01,
            "chain {name}"
        );
    }
}

#[tokio::test]
async fn every_chain_reports_its_own_numbers() {
    let report = run_mixed([50.0, 30.0, 20.0]).await;
    assert_eq!(report.metrics.chains.len(), 3);
    for name in ["a", "b", "c"] {
        let chain = &report.metrics.chains[name];
        assert!(chain.duration.count() > 0, "chain {name} recorded nothing");
        assert_eq!(chain.steps.len(), 1);
        assert!(chain.steps["get"].completed > 0);
    }
    // And the run-wide counters are the sum of them, not one of them.
    let completed: u64 = report
        .metrics
        .chains
        .values()
        .map(|chain| chain.completed)
        .sum();
    assert_eq!(completed, report.metrics.counters.completed);
}

#[test]
fn percentages_that_do_not_total_a_hundred_are_refused_by_how_far_out_they_are() {
    let short = load_with(json!([
        {"name": "a", "percent": 60, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]},
        {"name": "b", "percent": 35, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]}
    ]))
    .unwrap_err();
    assert!(short.contains("short of"), "{short}");
    assert!(short.contains('5'), "{short}");

    let over = load_with(json!([
        {"name": "a", "percent": 60, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]},
        {"name": "b", "percent": 50, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]}
    ]))
    .unwrap_err();
    assert!(over.contains("over"), "{over}");
}

#[test]
fn a_share_that_cannot_be_written_exactly_is_still_accepted() {
    // 99.996 across six chains. The tolerance is the shared `PERCENT_EPSILON`, so
    // what the engine accepts is what the control plane accepts.
    let chains: Vec<Value> = (0..6)
        .map(|index| {
            json!({"name": format!("c{index}"), "percent": 16.666, "session": "fresh",
                   "steps": [{"id": "s", "call": "ping"}]})
        })
        .collect();
    load_with(json!(chains)).unwrap();
}

#[test]
fn a_chain_at_zero_is_refused_rather_than_silently_never_run() {
    let error = load_with(json!([
        {"name": "a", "percent": 100, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]},
        {"name": "b", "percent": 0, "session": "fresh", "steps": [{"id": "s", "call": "ping"}]}
    ]))
    .unwrap_err();
    // A chain that can never be selected is a chain nobody would see was missing.
    assert!(error.contains("never runs"), "{error}");
    assert!(error.contains("chains/1"), "{error}");
}
