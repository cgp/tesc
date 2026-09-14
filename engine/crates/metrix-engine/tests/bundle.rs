mod support;

use metrix_engine::Plan;
use serde_json::{Value, json};
use support::{bundle, edit};

#[test]
fn example_bundle_compiles() {
    Plan::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../examples/plans/mock-fixed")
            .as_path(),
    )
    .unwrap();
}

#[test]
fn accepts_default_idle_phases_and_optional_warmup() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "mix.json", |doc| {
        doc.as_object_mut().unwrap().remove("phases");
        doc["load"]["warmup"] = json!("10s");
    });
    Plan::load(dir.path()).unwrap();
    edit(dir.path(), "mix.json", |doc| {
        doc["load"].as_object_mut().unwrap().remove("warmup");
    });
    Plan::load(dir.path()).unwrap();
}

#[test]
fn rejects_unrepresentable_phase_timeline_before_network_io() {
    for phase in ["baseline", "settle"] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "mix.json", |doc| {
            doc["phases"][phase] = json!(format!("{}s", u64::MAX));
        });
        assert!(Plan::load(dir.path()).is_err());
    }
}

#[test]
fn rejects_invalid_and_future_features_before_network_io() {
    for (file, pointer, value) in [
        ("mix.json", "/version", json!(2)),
        ("mix.json", "/load/rate", json!(0)),
        ("mix.json", "/load/rate", json!(-1)),
        ("mix.json", "/load/duration", json!("0s")),
        ("mix.json", "/load/mode", json!("stages")),
        ("mix.json", "/load/model", json!("closed")),
        ("mix.json", "/load/max_concurrency", json!(0)),
        ("mix.json", "/engine/worker_threads", json!(0)),
        ("mix.json", "/engine/connections_per_host", json!(0)),
        ("mix.json", "/chains/0/percent", json!(99)),
        // A pooled chain with no population size: the pool is one session, which is
        // `reuse` wearing a different name.
        ("mix.json", "/chains/0/session", json!("pool")),
        ("mix.json", "/chains/0/steps/0/call", json!("missing")),
        ("mix.json", "/defaults/timeout_ms", json!(0)),
        ("targets.json", "/list/0/address", json!("localhost")),
        (
            "targets.json",
            "/list/0/address",
            json!("user:secret@localhost:80"),
        ),
        ("targets.json", "/list/0/http_version", json!("http3")),
        (
            "calls/ping.json",
            "/ping/path",
            json!("https://other.example/"),
        ),
        ("calls/ping.json", "/ping/path", json!("/{{ token }}")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), file, |doc| {
            *doc.pointer_mut(pointer).unwrap() = value
        });
        assert!(Plan::load(dir.path()).is_err(), "accepted {file}{pointer}");
    }
    for (file, edit_doc) in [
        (
            // A credential injected into a header the transport owns.
            "mix.json",
            json!({"auth": {
                "mode": "bearer", "token": "secret",
                "inject": {"header": "Host", "format": "{{ token }}"},
            }}),
        ),
        (
            "mix.json",
            json!({"slo": [{"metric": "unknown", "max": 0.1}]}),
        ),
        (
            // A selector with nothing asked of it: vacuously true, which is a plan
            // error wearing the clothes of a passing test.
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "assert": [{"json": "$.id"}]}}),
        ),
        (
            // A call built by a generator the mix never declared.
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "generate": {"generator": "gen"}}}),
        ),
        (
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "headers": {"Host": "other"}}}),
        ),
        (
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "headers": {"Content-Length": "123"}}}),
        ),
        (
            "targets.json",
            json!({"list": [{"id": "a", "address": "localhost:1", "tls": {"enabled": false, "insecure_skip_verify": true}}]}),
        ),
        (
            "targets.json",
            json!({"list": [{"id": "a", "address": "localhost:1", "host_header": "bad\r\nHost: injected"}]}),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), file, |doc| {
            doc.as_object_mut()
                .unwrap()
                .extend(edit_doc.as_object().unwrap().clone())
        });
        assert!(Plan::load(dir.path()).is_err(), "accepted {edit_doc}");
    }
}

#[test]
fn rejects_traversal_missing_and_duplicate_call_files() {
    for files in [
        json!(["../outside.json"]),
        json!(["missing.json"]),
        json!(["calls/ping.json", "calls/ping.json"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "mix.json", |doc| doc["calls"] = files);
        assert!(Plan::load(dir.path()).is_err());
    }
}

#[test]
fn validation_messages_do_not_echo_secrets_from_invalid_input() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "calls/ping.json", |doc| {
        doc["ping"]["timeout_ms"] = Value::String("top-secret-token".into())
    });
    let error = Plan::load(dir.path()).err().unwrap();
    assert!(error.contains("line") && !error.contains("top-secret"));
}

#[test]
fn validates_bundle_detector_settings_and_tiny_rates_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    for (tolerance, drift, accepted) in [
        (0, 1, true),
        (99, 3_600_000, true),
        (100, 1, false),
        (2, 0, false),
        (2, 3_600_001, false),
    ] {
        edit(dir.path(), "mix.json", |d| {
            d["engine"]["rate_tolerance_pct"] = json!(tolerance);
            d["engine"]["send_drift_threshold_ms"] = json!(drift);
        });
        assert_eq!(Plan::load(dir.path()).is_ok(), accepted);
    }
    edit(dir.path(), "mix.json", |d| {
        d["engine"]
            .as_object_mut()
            .unwrap()
            .remove("send_drift_threshold_ms");
        d["engine"]["rate_tolerance_pct"] = json!(2);
        d["load"]["rate"] = json!(1e-300);
    });
    assert_eq!(
        Plan::load(dir.path())
            .unwrap()
            .detector_config
            .drift_threshold,
        std::time::Duration::from_secs(3600)
    );
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_the_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    std::fs::remove_file(dir.path().join("calls/ping.json")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("calls/ping.json")).unwrap();
    assert!(Plan::load(dir.path()).err().unwrap().contains("escapes"));
}

/// B3.1: a `call` reference is resolved wherever it is written, not only where the
/// executor happens to look. The chains below are all refused for *other* reasons
/// until B3.2 and B3.3 land -- what is checked here is that the reference itself is
/// judged first, and that the message names the step that wrote it.
mod resolution {
    use super::*;

    /// Two chains, the second naming a call that does not exist. Only the first
    /// would ever have been compiled before.
    fn two_chains(second_call: &str) -> Value {
        json!([
            {"name": "a", "percent": 50, "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
            {"name": "b", "percent": 50, "session": "fresh",
             "steps": [{"id": "get", "call": second_call}]}
        ])
    }

    fn load_with(chains: Value) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "mix.json", |doc| doc["chains"] = chains);
        Plan::load(dir.path()).map(drop)
    }

    #[test]
    fn a_reference_in_a_later_chain_is_resolved_too() {
        let error = load_with(two_chains("nowhere")).unwrap_err();
        // The chain that named it, not "chain 0" and not a bare "unresolved".
        assert!(error.contains("chains/1/steps/0/call"), "{error}");
        assert!(error.contains("nowhere"), "{error}");
    }

    #[test]
    fn a_reference_is_judged_before_the_features_around_it() {
        // The second chain names a call that does not exist *and* asks for a session
        // policy that is not implemented. The reference is the one reported: getting
        // this order wrong tells somebody to change their session policy when their
        // real problem is a typo.
        let mut chains = two_chains("nowhere");
        chains[1]["session"] = json!("reuse");
        let error = load_with(chains).unwrap_err();
        assert!(error.contains("chains/1/steps/0/call"), "{error}");
    }

    #[test]
    fn two_chains_cannot_share_a_name() {
        let error = load_with(json!([
            {"name": "same", "percent": 50, "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
            {"name": "same", "percent": 50, "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]}
        ]))
        .unwrap_err();
        // The name keys every series and every SLO; two of them is a report that
        // cannot say which chain it is about.
        assert!(error.contains("chains/1/name"), "{error}");
    }

    #[test]
    fn two_steps_of_one_chain_cannot_share_an_id() {
        let error = load_with(json!([
            {"name": "a", "percent": 100, "session": "fresh",
             "steps": [{"id": "same", "call": "ping"}, {"id": "same", "call": "ping"}]}
        ]))
        .unwrap_err();
        assert!(error.contains("steps/1/id"), "{error}");
    }

    #[test]
    fn a_chain_with_no_steps_sends_nothing_and_says_so() {
        let error = load_with(json!([
            {"name": "a", "percent": 100, "session": "fresh", "steps": []}
        ]))
        .unwrap_err();
        assert!(error.contains("chains/0/steps"), "{error}");
    }

    #[test]
    fn a_call_nobody_invokes_is_not_compiled() {
        // It is read and parsed -- a malformed file is still a malformed file -- but
        // a path it declares is never sent, so refusing the run over it would be
        // refusing over a request that does not exist.
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "calls/ping.json", |doc| {
            doc["unused"] = json!({"method": "GET", "path": "/{{ never_bound }}"});
        });
        Plan::load(dir.path()).unwrap();
    }

    #[test]
    fn one_name_defined_in_two_files_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        support::write(
            dir.path(),
            "calls/again.json",
            &json!({"ping": {"method": "GET", "path": "/elsewhere"}}),
        );
        edit(dir.path(), "mix.json", |doc| {
            doc["calls"] = json!(["calls/ping.json", "calls/again.json"]);
        });
        let error = Plan::load(dir.path()).map(drop).unwrap_err();
        // A step naming it could not say which one it meant.
        assert!(error.contains("ping"), "{error}");
    }

    #[test]
    fn the_compiled_request_is_the_one_the_step_named() {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "calls/ping.json", |doc| {
            doc["other"] = json!({"method": "POST", "path": "/other"});
        });
        edit(dir.path(), "mix.json", |doc| {
            doc["chains"][0]["steps"][0]["call"] = json!("other");
        });
        let plan = Plan::load(dir.path()).unwrap();
        assert_eq!(plan.request_for_test().uri.path(), "/other");
        assert_eq!(plan.request_for_test().method, "POST");
    }
}
