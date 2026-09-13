mod support;

use metrix_engine::{Output, Plan};
use serde_json::{Value, json};
use std::{fs, process::Command};
use support::{bundle, edit};

#[test]
fn calibration_persists_a_matching_profile_and_refuses_excess_demand_before_output() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "targets.json", |doc| {
        doc["list"][0]["tls"] = json!({"enabled": true});
    });
    let binary = env!("CARGO_BIN_EXE_metrix-engine");
    let calibrated = Command::new(binary)
        .args(["--plan", dir.path().to_str().unwrap(), "--calibrate"])
        .output()
        .unwrap();
    assert!(
        calibrated.status.success(),
        "{}",
        String::from_utf8_lossy(&calibrated.stderr)
    );
    let profile: Value = serde_json::from_slice(&calibrated.stdout).unwrap();
    let ceiling = profile["ceilings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|point| point["worker_threads"] == 2)
        .unwrap()["loopback_rps"]
        .as_f64()
        .unwrap();
    assert!(ceiling > 0.0);
    assert!(dir.path().join("machine-profile.json").is_file());

    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(ceiling * 0.95)
    });
    let summary = dir.path().join("refused.ndjson");
    let refused = Command::new(binary)
        .arg("--plan")
        .arg(dir.path())
        .arg("--summary")
        .arg(&summary)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("calibrated generator ceiling"));
    assert!(
        !summary.exists(),
        "refusal must precede output and target setup"
    );
}

#[test]
fn overridden_headroom_is_exposed_in_lifecycle_and_summaries() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    let binary = env!("CARGO_BIN_EXE_metrix-engine");
    let calibrated = Command::new(binary)
        .args(["--plan", dir.path().to_str().unwrap(), "--calibrate"])
        .output()
        .unwrap();
    assert!(calibrated.status.success());
    let profile: Value = serde_json::from_slice(&calibrated.stdout).unwrap();
    let ceiling = profile["ceilings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|point| point["worker_threads"] == 2)
        .unwrap()["loopback_rps"]
        .as_f64()
        .unwrap();
    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(ceiling * 0.95);
        doc["engine"]["allow_generator_limited"] = json!(true);
    });
    let plan = Plan::load(dir.path()).unwrap();
    let summary = dir.path().join("headroom.ndjson");
    Output::open(&plan, &summary, None, 1.0, 0)
        .unwrap()
        .finish(0, None);
    let records: Vec<Value> = fs::read_to_string(summary)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["machine_profile"], profile["id"]);
    let annotation = records
        .iter()
        .find(|record| record["type"] == "annotation" && record["code"] == "generator_headroom")
        .unwrap();
    assert_eq!(annotation["severity"], "invalid");
    assert_eq!(annotation["detail"]["overridden"], true);
}

#[test]
fn mismatched_machine_profile_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    let profile = json!({
        "version": 1,
        "id": "sha256:not-a-profile",
        "hardware": {"architecture": "other", "logical_cores": 1, "physical_cores": 1},
        "shape": {"request_body_bytes": 0, "tls": false, "chain_depth": 1, "generation": "static"},
        "ceilings": [{"worker_threads": 2, "null_rps": 1.0, "loopback_rps": 1.0}]
    });
    fs::write(
        dir.path().join("machine-profile.json"),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
    assert!(
        Plan::load(dir.path())
            .err()
            .unwrap()
            .contains("does not match this machine")
    );
    let recalibrated = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .args(["--plan", dir.path().to_str().unwrap(), "--calibrate"])
        .output()
        .unwrap();
    assert!(recalibrated.status.success());
}
