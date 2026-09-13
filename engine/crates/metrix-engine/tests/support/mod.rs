use serde_json::{Value, json};
use std::{fs, net::SocketAddr, path::Path};

pub fn bundle(root: &Path, address: SocketAddr, http_version: &str) {
    fs::create_dir_all(root.join("calls")).unwrap();
    write(
        root,
        "mix.json",
        &json!({
            "version": 1, "name": "test", "calls": ["calls/ping.json"],
            "phases": {"baseline": "0s", "settle": "0s"},
            "defaults": {"timeout_ms": 1000},
        "load": {"mode": "fixed", "model": "open", "rate": 50, "duration": "1s", "max_concurrency": 20},
            "engine": {"worker_threads": 2, "connections_per_host": 20},
            "chains": [{"name": "ping", "percent": 100, "session": "fresh", "steps": [{"id": "get", "call": "ping"}]}]
        }),
    );
    write(
        root,
        "targets.json",
        &json!({"list": [{"id": "mock", "address": address.to_string(), "http_version": http_version}]}),
    );
    write(
        root,
        "calls/ping.json",
        &json!({"ping": {"method": "GET", "path": "/ping"}}),
    );
}

pub fn write(root: &Path, name: &str, value: &Value) {
    fs::write(root.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

pub fn edit(root: &Path, name: &str, edit: impl FnOnce(&mut Value)) {
    let mut value = serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap();
    edit(&mut value);
    write(root, name, &value);
}
