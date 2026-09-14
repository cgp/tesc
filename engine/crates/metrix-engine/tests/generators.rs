//! B3.6: a request built by something other than substitution.
//!
//! Against a service that records the exact request, because the question a generator
//! test has to answer is what actually went on the wire — not that something did.

mod support;

use std::{
    convert::Infallible,
    fs,
    future::pending,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
};

use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, run};
use serde_json::{Value, json};
use support::{bundle, edit};
use tokio::net::TcpListener;

/// Method, target and body of every request, in the order it was answered.
type Seen = Arc<Mutex<Vec<Sent>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sent {
    target: String,
    body: String,
    content_type: String,
}

async fn serve(seen: Seen) -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let target = request.uri().to_string();
                        let content_type = request
                            .headers()
                            .get("content-type")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let body = request
                            .into_body()
                            .collect()
                            .await
                            .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
                            .unwrap_or_default();
                        seen.lock().expect("the recorder").push(Sent {
                            target,
                            body,
                            content_type,
                        });
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"ok")))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

/// A bundle whose one call is built by a Lua script.
fn with_lua(root: &Path, address: SocketAddr, script: &str, call: Value) {
    bundle(root, address, "http1");
    fs::create_dir_all(root.join("gen")).unwrap();
    fs::write(root.join("gen/build.lua"), script).unwrap();
    support::write(root, "calls/ping.json", &json!({"only": call}));
    edit(root, "mix.json", |doc| {
        doc["generators"] =
            json!({"build": {"type": "lua", "file": "gen/build.lua", "entry": "generate"}});
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh",
            "steps": [{"id": "only", "call": "only"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
}

const ECHO: &str = r#"
function generate(ctx)
  return {
    path = "/o/" .. ctx.args.prefix .. "-" .. ctx.iteration,
    query = { tier = ctx.args.tier, n = ctx.rng:int(1, 3) },
    headers = { ["X-Built-By"] = "lua" },
    body = "<order><step>" .. ctx.step .. "</step></order>"
  }
end
"#;

#[tokio::test]
async fn a_script_builds_the_whole_request_not_only_its_body() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        ECHO,
        json!({
            "method": "POST",
            "path": "/unused",
            "headers": {"Content-Type": "application/xml"},
            "generate": {"generator": "build", "args": {"prefix": "A", "tier": "gold"}},
        }),
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    for sent in &seen {
        // Path, query, headers and body: the hook returns a request, and a hook that
        // only returned a body could not express most of what generators are for.
        assert!(sent.target.starts_with("/o/A-"), "{sent:?}");
        assert!(sent.target.contains("tier=gold"), "{sent:?}");
        assert!(sent.body.contains("<step>only</step>"), "{sent:?}");
        // What the call declared and the hook did not return is kept.
        assert_eq!(sent.content_type, "application/xml");
    }
    // The iteration number reaches the script, so a plan can build a request that is
    // unique to the iteration without the engine knowing what unique means here.
    let targets: std::collections::BTreeSet<_> =
        seen.iter().map(|sent| sent.target.clone()).collect();
    assert_eq!(targets.len(), seen.len());
}

#[tokio::test]
async fn a_generated_request_is_the_same_on_a_replay() {
    let send = |seed| async move {
        let seen: Seen = Arc::default();
        let address = serve(Arc::clone(&seen)).await;
        let dir = tempfile::tempdir().unwrap();
        with_lua(
            dir.path(),
            address,
            ECHO,
            json!({
                "method": "POST",
                "path": "/unused",
                "generate": {"generator": "build", "args": {"prefix": "A", "tier": "gold"}},
            }),
        );
        let mut plan = Plan::load(dir.path()).unwrap();
        plan.set_seed(seed);
        run(plan, pending::<()>()).await.unwrap();
        let mut sent = seen.lock().unwrap().clone();
        sent.sort_by(|a, b| a.target.cmp(&b.target));
        sent
    };
    // `ctx.rng` draws from the iteration's own stream, so a generated request replays
    // with the rest of the plan rather than being the one unrepeatable part of it.
    assert_eq!(send(5).await, send(5).await);
    assert_ne!(send(5).await, send(6).await);
}

#[tokio::test]
async fn a_dataset_row_reaches_the_script_and_its_arguments() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        r#"
function generate(ctx)
  return { path = "/u/" .. ctx.rows.users.email .. "/" .. ctx.args.who }
end
"#,
        json!({
            "method": "GET",
            "path": "/unused",
            "generate": {"generator": "build", "args": {"who": "{{ users.email }}"}},
        }),
    );
    fs::create_dir_all(dir.path().join("data")).unwrap();
    fs::write(
        dir.path().join("data/users.csv"),
        "email,region\na@x.test,apac\nb@x.test,emea\n",
    )
    .unwrap();
    edit(dir.path(), "mix.json", |doc| {
        doc["datasets"] = json!({"users": {"file": "data/users.csv", "mode": "round_robin"}});
    });
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    for sent in &seen {
        // Both ways of reaching the row agree, which is the point: a templated
        // argument lets a call hand its script a value without the script knowing
        // datasets exist, and `ctx.rows` is there for a script that wants the row.
        let (_, rest) = sent.target.split_once("/u/").expect("a generated path");
        let (from_row, from_args) = rest.split_once('/').expect("both halves");
        assert_eq!(from_row, from_args, "{sent:?}");
    }
}

#[tokio::test]
async fn a_script_that_fails_is_the_plan_s_failure_and_not_the_service_s() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        "function generate(ctx) error('no id for this one') end",
        json!({
            "method": "GET",
            "path": "/unused",
            "generate": {"generator": "build"},
        }),
    );
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    assert!(seen.lock().unwrap().is_empty(), "something was sent");
    let step = &report.metrics.chains["chain"].steps["only"];
    // Attempted and failed, under generation rather than under a transport class: the
    // service was never asked, and counting this against it would send somebody
    // looking for a fault in a system that did nothing wrong.
    assert!(step.failed > 0);
    assert_eq!(step.errors[9], step.failed, "generation failures");
    assert_eq!(report.timed_out, 0);
    assert_eq!(report.metrics.chains["chain"].aborted, step.failed);
}

#[tokio::test]
async fn what_a_script_costs_is_measured_as_the_script_s_own_time() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        r#"
function generate(ctx)
  local total = 0
  for i = 1, 20000 do total = total + i end
  return { path = "/o/" .. total }
end
"#,
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    let counts = &report.metrics.generation["build"];
    // Counted per generator and kept out of the step's latency: §7.3 says a slow
    // generator has to be visible as a slow generator, and folded into response time
    // it would read as a slow endpoint instead.
    assert_eq!(counts.calls, report.metrics.chains["chain"].started);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.duration.count(), counts.calls);
}

#[tokio::test]
async fn a_script_cannot_reach_the_machine_it_runs_on() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    for reaching in [
        "os.execute('echo hi')",
        "local f = io.open('x', 'w')",
        "require('os')",
        "load('return 1')()",
    ] {
        let dir = tempfile::tempdir().unwrap();
        with_lua(
            dir.path(),
            address,
            &format!("function generate(ctx) {reaching} return {{}} end"),
            json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
        );
        // The script compiles; what it cannot do is call any of these. A generator
        // that needs to write, execute or reach the network wants the exec tier,
        // where the process boundary makes the cost and the risk explicit.
        let plan = Plan::load(dir.path()).unwrap();
        let report = run(plan, pending::<()>()).await.unwrap();
        let step = &report.metrics.chains["chain"].steps["only"];
        assert_eq!(step.errors[9], step.failed, "{reaching}");
        assert!(step.failed > 0, "{reaching} was allowed");
    }
}

#[test]
fn a_script_that_will_not_compile_is_refused_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function generate(ctx) return { ",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    // At load, not at request one: a run that starts and fails everything has spent
    // the window it was given.
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // Named by the file, which is what there is to go and open.
    assert!(error.contains("build.lua"), "{error}");
}

#[test]
fn a_script_with_no_entry_function_says_which_name_it_looked_for() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function build(ctx) return {} end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("generate"), "{error}");
}

#[test]
fn a_call_naming_a_generator_nobody_declared_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function generate(ctx) return {} end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "buidl"}}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(
        error.contains("buidl") && error.contains("build"),
        "{error}"
    );
}

#[test]
fn a_plugin_this_binary_does_not_carry_is_refused_with_what_it_does() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function generate(ctx) return {} end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "signer"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"]["signer"] = json!({"type": "plugin", "name": "hmac"});
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // A plugin is part of the binary, so a plan that needs one needs a build with it.
    assert!(error.contains("compiled into this engine"), "{error}");
}

#[test]
fn building_requests_ahead_says_why_it_is_not_available() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function generate(ctx) return {} end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"]["build"]["prefetch"] = json!(64);
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    // Refused rather than approximated: a buffered request carries the iteration it
    // was built for, and handing it to a later one makes the run unreplayable.
    assert!(error.contains("unreplayable"), "{error}");
}

#[test]
fn a_script_outside_the_bundle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        "function generate(ctx) return {} end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    fs::write(
        dir.path().parent().unwrap().join("outside.lua"),
        "function generate(ctx) return {} end",
    )
    .unwrap();
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"]["build"]["file"] = json!("../outside.lua");
    });
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("outside the bundle"), "{error}");
}

#[tokio::test]
async fn a_returned_path_is_checked_before_it_reaches_the_transport() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        "function generate(ctx) return { path = 'orders/1' } end",
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    assert!(seen.lock().unwrap().is_empty(), "a bad path was sent");
    // Caught as generation rather than reaching `Uri::parse` inside the send path,
    // where the failure would be recorded against the service.
    let step = &report.metrics.chains["chain"].steps["only"];
    assert_eq!(step.errors[9], step.failed);
    assert!(step.failed > 0);
}

#[tokio::test]
async fn a_sidecar_answers_over_its_pipe() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), address, "http1");
    fs::write(
        dir.path().join("sidecar.py"),
        r#"
import json, sys
for line in sys.stdin:
    ctx = json.loads(line)
    print(json.dumps({
        "path": "/s/%d" % ctx["iteration"],
        "body": ctx["args"].get("what", ""),
    }), flush=True)
"#,
    )
    .unwrap();
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"only": {
            "method": "POST",
            "path": "/unused",
            "generate": {"generator": "side", "args": {"what": "payload"}},
        }}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"] = json!({"side": {
            "type": "exec",
            "command": ["python", "sidecar.py"],
            "protocol": "ndjson",
            "pool": 2,
            "timeout_ms": 5000,
        }});
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh",
            "steps": [{"id": "only", "call": "only"}]
        }]);
        doc["load"]["rate"] = json!(10);
        doc["load"]["duration"] = json!("1s");
    });
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    for sent in &seen {
        assert!(sent.target.starts_with("/s/"), "{sent:?}");
        assert_eq!(sent.body, "payload");
    }
    // A pool of long-lived processes: the same two answered every request, so the
    // measured window paid for one fork each rather than one per request.
    assert_eq!(report.metrics.generation["side"].failed, 0);
    assert_eq!(
        report.metrics.generation["side"].calls,
        report.metrics.chains["chain"].started
    );
}

#[test]
fn a_sidecar_that_cannot_be_started_is_refused_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    support::write(
        dir.path(),
        "calls/ping.json",
        &json!({"only": {
            "method": "GET", "path": "/unused",
            "generate": {"generator": "side"},
        }}),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"] = json!({"side": {
            "type": "exec", "command": ["definitely-not-a-program-9f3a"],
        }});
        doc["chains"] = json!([{
            "name": "chain", "percent": 100, "session": "fresh",
            "steps": [{"id": "only", "call": "only"}]
        }]);
    });
    let plan = Plan::load(dir.path()).expect("the command is not checked until it is run");
    // Started before the arrival clock, so a sidecar that will not start stops the
    // run rather than failing every request in it.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(run(plan, pending::<()>()))
        .map(drop)
        .unwrap_err();
    assert!(error.contains("definitely-not-a-program-9f3a"), "{error}");
}

/// A bundle whose script reads a corpus of static files.
fn with_corpus(root: &Path, address: SocketAddr, script: &str, files: &[(&str, &str)]) {
    with_lua(
        root,
        address,
        script,
        json!({"method": "POST", "path": "/unused", "generate": {"generator": "build"}}),
    );
    for (name, contents) in files {
        let path = root.join("corpus").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    edit(root, "mix.json", |doc| {
        doc["generators"]["build"]["corpus"] = json!({"dir": "corpus"});
    });
}

#[tokio::test]
async fn a_script_sends_a_payload_it_read_from_the_bundle() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        address,
        r#"
function generate(ctx)
  local name = corpus_names[ctx.rng:int(1, #corpus_names)]
  return { path = "/p", body = corpus[name] }
end
"#,
        &[
            ("payloads/one.xml", "<order>1</order>"),
            ("payloads/two.xml", "<order>2</order>"),
        ],
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    let bodies: std::collections::BTreeSet<_> = seen.iter().map(|sent| sent.body.clone()).collect();
    // Both files reachable, and nothing else: the corpus is what is under the
    // directory and the script picked from it by index.
    assert!(
        bodies.iter().all(|body| body.starts_with("<order>")),
        "{bodies:?}"
    );
    assert!(
        bodies.len() > 1,
        "only one payload was ever picked: {bodies:?}"
    );
}

#[tokio::test]
async fn a_corpus_is_read_once_rather_than_per_request() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        address,
        r#"function generate(ctx) return { path = "/p", body = corpus["a.txt"] } end"#,
        &[("a.txt", "original")],
    );
    let plan = Plan::load(dir.path()).unwrap();
    // Changed on disk after the plan is loaded. The run must not notice, because
    // nothing opens the file again: no file handles on the hot path.
    fs::write(dir.path().join("corpus/a.txt"), "changed-mid-run").unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    assert!(seen.iter().all(|sent| sent.body == "original"), "{seen:?}");
}

#[tokio::test]
async fn a_script_cannot_write_to_the_corpus_it_shares_with_the_next_iteration() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        address,
        r#"function generate(ctx) corpus["a.txt"] = "mine" return { path = "/p" } end"#,
        &[("a.txt", "shared")],
    );
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    assert!(seen.lock().unwrap().is_empty(), "the write was allowed");
    // A VM outlives the iteration that used it, so a script that could write to the
    // corpus would be leaking one iteration's state into the next.
    let step = &report.metrics.chains["chain"].steps["only"];
    assert_eq!(step.errors[9], step.failed);
    assert!(step.failed > 0);
}

#[tokio::test]
async fn asking_for_a_file_that_is_not_there_says_what_is() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        address,
        r#"function generate(ctx) return { path = "/p", body = corpus["b.txt"] } end"#,
        &[("a.txt", "only this one")],
    );
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();

    // Rather than a nil body reaching the service as a hole in the request.
    assert!(seen.lock().unwrap().is_empty(), "a nil body was sent");
    let step = &report.metrics.chains["chain"].steps["only"];
    assert_eq!(step.errors[9], step.failed);
}

#[test]
fn a_corpus_over_its_ceiling_is_refused_with_both_numbers() {
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        r#"function generate(ctx) return { path = "/p" } end"#,
        &[("big.txt", &"x".repeat(40 * 1024))],
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"]["build"]["corpus"] = json!({"dir": "corpus", "max_kb": 16});
    });
    // A corpus is held in memory for the whole run, so the ceiling is what stops a
    // plan pointing at a build directory from taking the box with it.
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("16 KB"), "{error}");
}

#[test]
fn a_corpus_outside_the_bundle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_corpus(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        r#"function generate(ctx) return { path = "/p" } end"#,
        &[("a.txt", "x")],
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["generators"]["build"]["corpus"] = json!({"dir": "../elsewhere"});
    });
    fs::create_dir_all(dir.path().parent().unwrap().join("elsewhere")).unwrap();
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("outside the bundle"), "{error}");
}

#[tokio::test]
async fn a_script_with_no_corpus_has_no_corpus_globals() {
    let seen: Seen = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    with_lua(
        dir.path(),
        address,
        r#"function generate(ctx) return { path = "/p/" .. tostring(corpus == nil) } end"#,
        json!({"method": "GET", "path": "/unused", "generate": {"generator": "build"}}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    run(plan, pending::<()>()).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "nothing was sent");
    // Absent rather than empty: a plan that declared no corpus has none, and a script
    // reading one is asking about something the plan never said existed.
    assert!(seen.iter().all(|sent| sent.target == "/p/true"), "{seen:?}");
}
