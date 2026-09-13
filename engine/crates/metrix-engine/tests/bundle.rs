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
        ("mix.json", "/phases/baseline", json!("30s")),
        ("mix.json", "/chains/0/percent", json!(99)),
        ("mix.json", "/chains/0/session", json!("reuse")),
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
            "mix.json",
            json!({"auth": {"type": "bearer", "token": "secret"}}),
        ),
        (
            "mix.json",
            json!({"slo": [{"metric": "error_rate", "max": 0.1}]}),
        ),
        (
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "assert": [{"status": 200}]}}),
        ),
        (
            "calls/ping.json",
            json!({"ping": {"method": "GET", "path": "/", "body": {"generator": "gen"}}}),
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
            json!({"list": [{"id": "a", "address": "localhost:1", "tls": {"enabled": true, "insecure_skip_verify": true}}]}),
        ),
        (
            "targets.json",
            json!({"list": [{"id": "a", "address": "localhost:1", "host_header": "other"}]}),
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
