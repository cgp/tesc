//! B4.6: what CI reads -- the verdicts, and the exit code that carries them.

mod support;
use metrix_engine::{Plan, Report};
use metrix_metrics::events::{Bound, SloVerdict};
use metrix_mock::{Config, MockServer};
use serde_json::json;
use std::{future::pending, process::Command};

#[test]
fn checkout_example_compiles_with_explicit_runtime_credentials() {
    if std::env::var_os("METRIX_TEST_CHECKOUT_INPUTS").is_some() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../examples/plans/checkout-mixed");
        Plan::load(&root).unwrap();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "checkout_example_compiles_with_explicit_runtime_credentials",
            "--nocapture",
        ])
        .env("METRIX_TEST_CHECKOUT_INPUTS", "1")
        .env("CLIENT_ID", "test-client")
        .env("METRIX_SECRET_staging_client_secret", "test-secret")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn cli_verdicts_match_exit_codes_and_unsupported_tails_are_advisory() {
    for (breach, unsupported) in [(false, false), (true, false), (false, true)] {
        let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
        support::edit(dir.path(), "mix.json", |d| {
            d["slo"] = if unsupported {
                json!([{"metric":"p99_latency_ms","max":0}])
            } else {
                json!([{"metric":"error_rate","max":0.01}])
            }
        });
        if breach {
            support::edit(dir.path(), "calls/ping.json", |d| {
                d["ping"]["assert"] = json!([{"status":201}])
            });
        }
        let task = tokio::spawn(server.run_until(pending()));
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
            .arg("--plan")
            .arg(dir.path())
            .output()
            .await
            .unwrap();
        let expected = if breach { 2 } else { 0 };
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let verdict = records
            .iter()
            .find(|r| r["type"] == "run_finished")
            .unwrap();
        assert_eq!(verdict["exit_code"].as_i64(), Some(expected.into()));
        assert_eq!(verdict["slo"][0]["passed"].as_bool(), Some(!breach));
        assert_eq!(verdict["slo"][0]["supported"].as_bool(), Some(!unsupported));
        // Which side of the threshold: a floor and a ceiling can carry the same
        // number, and a reader cannot act on a direction it has to guess.
        assert_eq!(verdict["slo"][0]["bound"], "max");
        assert!(records.iter().any(|r| r["code"] == "slo_verdicts"
            && r["detail"]["evidence"][0]["requests"].as_u64().is_some()));
        task.abort();
    }
}

#[tokio::test]
async fn a_sweep_breach_survives_a_later_healthy_target() {
    let bad = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            capacity_rps: Some(1),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let good = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), bad.local_addr().unwrap(), "http1");
    support::write(
        dir.path(),
        "targets.json",
        &json!({"list":[{"id":"bad","address":bad.local_addr().unwrap().to_string()},{"id":"good","address":good.local_addr().unwrap().to_string()}]}),
    );
    support::edit(dir.path(), "calls/ping.json", |d| {
        d["ping"]["assert"] = json!([{"status":200}])
    });
    support::edit(dir.path(), "mix.json", |d| {
        d["slo"] = json!([{"metric":"error_rate","max":0.01}])
    });
    let bad = tokio::spawn(bad.run_until(pending()));
    let good = tokio::spawn(good.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let finish = records
        .iter()
        .find(|r| r["type"] == "run_finished")
        .unwrap();
    assert!(finish["slo"][0]["observed"].as_f64().unwrap() > 0.5);
    assert_eq!(
        records
            .iter()
            .filter(|r| r["type"] == "target_finished")
            .count(),
        2
    );
    bad.abort();
    good.abort();
}

#[tokio::test]
async fn generator_failure_has_a_nonzero_ci_code_and_no_capacity_claim() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: metrix_mock::Latency::Fixed { ms: 500.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["max_concurrency"] = json!(1);
        d["load"]["breakpoint"] =
            json!({"start_rate":50,"step_rate":50,"max_rate":100,"step_duration":"4s"});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(records.iter().any(|r| r["type"] == "run_finished"
        && r["exit_code"] == 3
        && r["stopped_because"] == "generator_limited"));
    assert!(
        records.iter().any(
            |r| r["code"] == "breakpoint_report" && r["detail"]["max_sustained_rate"].is_null()
        )
    );
    task.abort();
}

#[tokio::test]
async fn unsupported_breakpoint_slo_refuses_a_capacity_claim() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["slo"] = json!([{"metric":"p99_latency_ms","max":100}]);
        d["load"]["mode"] = json!("breakpoint");
        d["load"]["breakpoint"] =
            json!({"start_rate":10,"step_rate":10,"max_rate":20,"step_duration":"1s"});
    });
    let task = tokio::spawn(server.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|r| r["type"] == "run_finished" && r["stopped_because"] == "insufficient_samples")
    );
    assert!(
        records.iter().any(
            |r| r["code"] == "breakpoint_report" && r["detail"]["max_sustained_rate"].is_null()
        )
    );
    task.abort();
}

#[tokio::test]
async fn fixed_generator_cap_invalidates_ci_after_its_full_window() {
    let server = MockServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config {
            latency: metrix_mock::Latency::Fixed { ms: 500.0 },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["load"]["max_concurrency"] = json!(1)
    });
    let task = tokio::spawn(server.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    task.abort();
}

/// §16 names what invalidates a fixed run, and a chain that broke is not on the
/// list. Reporting one as `generator_limited` blames the load generator for a body
/// the service did not send, and buries the breach behind an abort.
#[tokio::test]
async fn a_chain_that_breaks_is_a_finding_about_the_service() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({
            // The mock answers `{"ok":true}`, so there is no id to carry forward.
            "first": {"method": "GET", "path": "/ping", "extract": {"id": {"json": "$.id"}}},
            "second": {"method": "GET", "path": "/ping/{{ id }}"},
        }),
    );
    support::edit(dir.path(), "mix.json", |d| {
        d["slo"] = json!([{"metric": "error_rate", "max": 0.01}]);
        d["chains"] = json!([{
            "name": "ping", "percent": 100, "session": "fresh",
            "steps": [{"id": "first", "call": "first"}, {"id": "second", "call": "second"}],
        }]);
    });
    let task = tokio::spawn(server.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !records.iter().any(|r| r["code"] == "generator_limited"),
        "a broken chain was reported as a generator that could not keep up"
    );
    let finish = records
        .iter()
        .find(|r| r["type"] == "run_finished")
        .unwrap();
    assert_eq!(finish["slo"][0]["passed"], json!(false));
    assert!(finish["slo"][0]["observed"].as_f64().unwrap() > 0.4);
    assert!(finish["stopped_because"].is_null(), "{finish}");
    task.abort();
}

/// The observer lives in the control plane, and the engine must not grow a
/// dependency on it to accept a plan that asks for one.
#[tokio::test]
async fn an_observe_block_is_accepted_and_says_who_owns_it() {
    let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), server.local_addr().unwrap(), "http1");
    support::edit(dir.path(), "mix.json", |d| {
        d["observe"] = json!({"interval_ms": 1000, "collect": ["cpu", "memory"]})
    });
    let task = tokio::spawn(server.run_until(pending()));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .output()
        .await
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Annotated rather than silently ignored: a run whose host samples are missing
    // must say so, or the gap reads as a host that had nothing to report.
    assert!(
        records
            .iter()
            .any(|r| r["code"] == "observation_unavailable")
    );
    task.abort();
}

fn verdict(passed: bool, supported: bool) -> SloVerdict {
    SloVerdict {
        metric: "p99_latency_ms".into(),
        chain: None,
        bound: Bound::Max,
        passed,
        observed: 1.0,
        threshold: 1.0,
        supported,
    }
}

/// The whole of §16's exit-code sentence in one place, because every one of these
/// means something different to the pipeline that reads it.
#[test]
fn the_exit_code_table_is_the_one_ci_reads() {
    let pass = Report::default();
    assert_eq!(pass.exit_code(), 0);

    // Advisory: the run did not find the service fast enough, it found that it
    // could not tell. That is not a failure anyone can act on.
    let advisory = Report {
        slo: vec![verdict(true, false)],
        ..Report::default()
    };
    assert_eq!(advisory.exit_code(), 0);

    let breach = Report {
        slo: vec![verdict(false, true)],
        ..Report::default()
    };
    assert_eq!(breach.exit_code(), 2);

    // A probe that stopped where the plan told it to stop did its job. The rate it
    // stopped at is the answer, not an error.
    let found_the_ceiling = Report {
        stopped_because: Some("error_rate".into()),
        ..Report::default()
    };
    assert_eq!(found_the_ceiling.exit_code(), 0);

    // A breach measured by a run that was already invalid is not a breach anyone
    // can act on, so the invalidity outranks it.
    for stopped in [None, Some("insufficient_samples".to_owned())] {
        let invalid = Report {
            generator_limited: stopped.is_none(),
            stopped_because: stopped,
            slo: vec![verdict(false, true)],
            ..Report::default()
        };
        assert_eq!(invalid.exit_code(), 3);
    }

    let interrupted = Report {
        interrupted: true,
        generator_limited: true,
        slo: vec![verdict(false, true)],
        ..Report::default()
    };
    assert_eq!(interrupted.exit_code(), 130);
}
