//! B3.11: errors that name a file and a place in it, and running one chain alone.

mod support;

use std::{convert::Infallible, fs, net::SocketAddr};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::Plan;
use metrix_metrics::events::Record;
use serde_json::json;
use support::{bundle, edit};
use tokio::net::TcpListener;
use tokio::process::Command;

/// Every load error is `<file>#<json-pointer>: <message>`.
///
/// A pointer rather than prose because the control plane shows these against the
/// field the reader is editing (§4.5), and prose is not something it can locate.
fn located(error: &str) -> (String, String) {
    let (head, _) = error.split_once(": ").unwrap_or((error, ""));
    let (file, pointer) = head
        .split_once('#')
        .unwrap_or_else(|| panic!("no file and pointer in {error:?}"));
    assert!(
        pointer.is_empty() || pointer.starts_with('/'),
        "{pointer:?} is not a JSON Pointer"
    );
    (file.to_owned(), pointer.to_owned())
}

#[test]
fn a_bad_field_in_the_mix_is_named_by_where_it_is() {
    for (pointer, value, expected) in [
        ("/load/rate", json!(0), "/load/rate"),
        // The list, because a total is a property of the list and not of one chain.
        ("/chains/0/percent", json!(99), "/chains"),
        ("/engine/worker_threads", json!(0), "/engine/worker_threads"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
        edit(dir.path(), "mix.json", |doc| {
            *doc.pointer_mut(pointer).unwrap() = value
        });
        let error = Plan::load(dir.path()).map(drop).unwrap_err();
        let (file, at) = located(&error);
        assert_eq!(file, "mix.json", "{error}");
        assert_eq!(at, expected, "{error}");
    }
}

#[test]
fn the_chain_a_message_is_about_is_the_one_it_points_at() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "mix.json", |doc| {
        // The percentages still total 100, so what is wrong is the second chain and
        // not the mixture: a pointer at the list would send the reader to the wrong
        // line.
        doc["chains"] = json!([
            {"name": "a", "percent": 100, "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
            {"name": "b", "percent": 0, "session": "fresh",
             "steps": [{"id": "get", "call": "ping"}]},
        ]);
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert_eq!(
        located(&error),
        ("mix.json".into(), "/chains/1/percent".into())
    );
}

#[test]
fn a_bad_field_in_a_call_names_the_file_that_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "calls/ping.json", |doc| {
        doc["ping"]["path"] = json!("not-a-path")
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    let (file, at) = located(&error);
    // A bundle can hold a dozen call files; naming the call alone says nothing about
    // which one to open.
    assert_eq!(file, "calls/ping.json", "{error}");
    assert_eq!(at, "/ping/path", "{error}");
}

#[test]
fn a_call_name_with_a_slash_in_it_is_still_a_pointer() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"api/ping": {"method": "GET", "path": "bad"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["chains"][0]["steps"][0]["call"] = json!("api/ping")
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    let (_, at) = located(&error);
    // `/` and `~` are the two characters a pointer cannot hold literally (RFC 6901),
    // and a call may be named anything.
    assert_eq!(at, "/api~1ping/path", "{error}");
}

#[test]
fn a_bad_target_is_named_in_its_own_file() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    edit(dir.path(), "targets.json", |doc| {
        doc["list"][0]["address"] = json!("no-port")
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    let (file, at) = located(&error);
    assert_eq!(file, "targets.json", "{error}");
    assert_eq!(at, "/list/0/address", "{error}");
}

async fn serve() -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = service_fn(|_: Request<hyper::body::Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"ok")))
                            .unwrap(),
                    )
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

fn mixture(root: &std::path::Path, address: SocketAddr) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({
            "big": {"method": "GET", "path": "/big"},
            "rare": {"method": "GET", "path": "/rare"},
        }),
    );
    edit(root, "mix.json", |doc| {
        doc["chains"] = json!([
            {"name": "browse", "percent": 97, "session": "fresh",
             "steps": [{"id": "get", "call": "big"}]},
            {"name": "checkout", "percent": 3, "session": "fresh",
             "steps": [{"id": "get", "call": "rare"}]},
        ]);
        doc["load"]["rate"] = json!(20);
        doc["load"]["duration"] = json!("1s");
    });
}

#[tokio::test]
async fn one_chain_can_be_run_on_its_own_at_the_whole_rate() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    mixture(dir.path(), address);
    let summary = dir.path().join("summary.ndjson");
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--chain")
        .arg("checkout")
        .arg("--summary")
        .arg(&summary)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let records: Vec<Record> = fs::read_to_string(&summary)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let chains: Vec<&str> = records
        .iter()
        .filter_map(|record| match record {
            Record::Summary(summary) => Some(summary.chains.keys().map(String::as_str)),
            _ => None,
        })
        .flatten()
        .collect();
    // Only the chain that was asked for, and it ran at the whole rate rather than at
    // its 3%: finding out whether it works should not take four minutes.
    assert!(chains.iter().all(|name| *name == "checkout"), "{chains:?}");
    assert!(!chains.is_empty(), "nothing ran");
    let started: u64 = records
        .iter()
        .filter_map(|record| match record {
            Record::Summary(summary) => summary
                .chains
                .get("checkout")
                .map(|chain| chain.iterations_started),
            _ => None,
        })
        .sum();
    assert!(started > 10, "only {started} iterations at the whole rate");

    // And the run says it was narrowed. A stored run of one chain out of two is not a
    // run of the mixture, and comparing it with one that was would be comparing two
    // different workloads.
    let narrowed = records
        .iter()
        .any(|record| matches!(record, Record::Annotation(note) if note.code == "single_chain"));
    assert!(narrowed, "the run did not say it was narrowed");
}

#[tokio::test]
async fn asking_for_a_chain_that_is_not_there_says_what_is() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    mixture(dir.path(), address);
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--chain")
        .arg("checkuot")
        .arg("--summary")
        .arg(dir.path().join("summary.ndjson"))
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("browse") && stderr.contains("checkout"),
        "{stderr}"
    );
}

/// A run narrowed to one chain must not fail CI for a chain it was told not to run.
#[tokio::test]
async fn narrowing_to_one_chain_drops_the_thresholds_about_the_others() {
    let address = serve().await;
    let dir = tempfile::tempdir().unwrap();
    mixture(dir.path(), address);
    edit(dir.path(), "mix.json", |doc| {
        doc["slo"] = json!([
            {"metric": "achieved_rate", "chain": "browse", "min": 1},
            {"metric": "achieved_rate", "chain": "checkout", "min": 1},
        ])
    });
    let output = Command::new(env!("CARGO_BIN_EXE_metrix-engine"))
        .arg("--plan")
        .arg(dir.path())
        .arg("--chain")
        .arg("checkout")
        .arg("--summary")
        .arg(dir.path().join("summary.ndjson"))
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
