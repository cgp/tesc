//! B3.8: the `auth` block, and keeping it out of the measurement.
//!
//! Run against two services: a target that checks the credential it is sent, and an
//! identity provider that issues them and counts how often it is asked. That second
//! count is most of the point — a generator whose refresh is not single-flight looks
//! correct from the target's side and floods the IdP.

mod support;

use std::{
    convert::Infallible,
    fs,
    future::pending,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, Report, run};
use serde_json::{Value, json};
use support::{bundle, edit};
use tokio::net::TcpListener;

/// What the identity provider did, and what the target saw.
#[derive(Default)]
struct Watched {
    /// Token requests. The number that matters for single-flight.
    issued: AtomicU64,
    /// Credentials the target was sent, in order.
    presented: Mutex<Vec<String>>,
    /// The oldest token generation still accepted. Raising it past `issued` revokes
    /// every credential in flight at once, which is what an expiry looks like from
    /// the outside: many users, one moment, one dead token.
    min_valid: AtomicU64,
    /// Revoke everything once this many requests have been answered. Zero never does.
    revoke_after: AtomicU64,
    /// Make issuing slow, so a second caller arrives while the first is still
    /// waiting. Without that there is nothing for single-flight to be single about.
    slow_issue_ms: AtomicU64,
    /// The bodies the token endpoint was posted, so a test can say which grant it was.
    grants: Mutex<Vec<String>>,
}

/// A service that issues tokens on `/token` and `/login`, and checks them everywhere
/// else.
async fn serve(watched: Arc<Watched>) -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let watched = Arc::clone(&watched);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let watched = Arc::clone(&watched);
                    async move {
                        let path = request.uri().path().to_owned();
                        let credential = request
                            .headers()
                            .get("authorization")
                            .or_else(|| request.headers().get("x-session-token"))
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let body = request
                            .into_body()
                            .collect()
                            .await
                            .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
                            .unwrap_or_default();
                        let response = match path.as_str() {
                            "/token" | "/login" => {
                                let delay = watched.slow_issue_ms.load(Ordering::SeqCst);
                                if delay > 0 {
                                    tokio::time::sleep(std::time::Duration::from_millis(delay))
                                        .await;
                                }
                                let issued = watched.issued.fetch_add(1, Ordering::SeqCst) + 1;
                                watched.grants.lock().unwrap().push(body);
                                let field = if path == "/token" {
                                    "access_token"
                                } else {
                                    "token"
                                };
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(format!(
                                        "{{\"{field}\":\"t-{issued}\",\"expires_in\":3600}}"
                                    ))))
                                    .unwrap()
                            }
                            _ => {
                                let seen = {
                                    let mut presented = watched.presented.lock().unwrap();
                                    presented.push(credential.clone());
                                    presented.len() as u64
                                };
                                let revoke = watched.revoke_after.load(Ordering::SeqCst);
                                if revoke > 0 && seen == revoke {
                                    watched.min_valid.store(
                                        watched.issued.load(Ordering::SeqCst) + 1,
                                        Ordering::SeqCst,
                                    );
                                }
                                let generation = credential
                                    .rsplit_once("t-")
                                    .and_then(|(_, n)| n.parse::<u64>().ok())
                                    .unwrap_or(0);
                                if generation < watched.min_valid.load(Ordering::SeqCst) {
                                    Response::builder()
                                        .status(401)
                                        .body(Full::new(Bytes::from_static(b"stale")))
                                        .unwrap()
                                } else {
                                    Response::builder()
                                        .status(200)
                                        .body(Full::new(Bytes::from_static(b"ok")))
                                        .unwrap()
                                }
                            }
                        };
                        Ok::<_, Infallible>(response)
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

fn with_auth(root: &Path, address: SocketAddr, auth: Value) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({"ping": {"method": "GET", "path": "/work"}}),
    );
    edit(root, "mix.json", |doc| {
        doc["auth"] = auth;
        doc["load"]["rate"] = json!(20);
        doc["load"]["duration"] = json!("1s");
    });
}

async fn run_bundle(root: &Path) -> Report {
    run(Plan::load(root).unwrap(), pending::<()>())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_token_is_fetched_before_the_run_and_sent_with_every_request() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{address}/token"),
            "client_id": "shop", "client_secret": "hunter2", "scope": "orders.write",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
        }),
    );
    let report = run_bundle(dir.path()).await;

    let presented = watched.presented.lock().unwrap().clone();
    assert!(!presented.is_empty(), "nothing was sent");
    // One token, fetched once, on every request. A token first fetched inside the
    // measured window would be a token fetch inside the measured window.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 1);
    assert!(
        presented.iter().all(|value| value == "Bearer t-1"),
        "{presented:?}"
    );
    let grant = watched.grants.lock().unwrap()[0].clone();
    assert!(grant.contains("grant_type=client_credentials"), "{grant}");
    assert!(grant.contains("scope=orders.write"), "{grant}");

    let auth = report.last_window.as_ref().unwrap().auth.unwrap();
    assert_eq!(auth.acquisitions, 1);
    assert_eq!(auth.identities, 1);
    assert_eq!(auth.refreshes, 0);
}

#[tokio::test]
async fn token_traffic_is_not_counted_as_load() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{address}/token"),
            "client_id": "shop", "client_secret": "hunter2",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
        }),
    );
    let report = run_bundle(dir.path()).await;

    // The IdP was asked, and none of it reached the load figures. Without this,
    // pointing a test at a protected service inflates its throughput with calls to a
    // different system.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 1);
    let step = &report.metrics.chains["ping"].steps["get"];
    assert_eq!(step.attempted, report.metrics.chains["ping"].started);
    assert_eq!(
        step.attempted,
        watched.presented.lock().unwrap().len() as u64
    );
    assert_eq!(report.sent, step.attempted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_of_401s_costs_one_refresh_rather_than_one_each() {
    let watched = Arc::new(Watched::default());
    // One dead token, many users, one moment — and an identity provider slow enough
    // that a second caller arrives while the first is still waiting on it.
    watched.revoke_after.store(20, Ordering::SeqCst);
    watched.slow_issue_ms.store(80, Ordering::SeqCst);
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{address}/token"),
            "client_id": "shop", "client_secret": "hunter2",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "refresh": {"on_401": "refresh_once"},
        }),
    );
    edit(dir.path(), "mix.json", |doc| {
        doc["load"]["rate"] = json!(200);
        doc["load"]["max_concurrency"] = json!(100);
        doc["engine"]["connections_per_host"] = json!(100);
    });
    let report = run_bundle(dir.path()).await;

    let issued = watched.issued.load(Ordering::SeqCst);
    let auth = report.last_window.as_ref().unwrap().auth.unwrap();
    assert!(auth.unauthorized >= 5, "the wall was never hit: {auth:?}");
    // One acquisition before the run and one refresh after the revocation, however
    // many users were rejected. The naive version sends one refresh per 401, floods
    // the identity provider, and produces a spike that reads as the target degrading.
    assert_eq!(
        issued, 2,
        "{issued} token requests for {} rejections",
        auth.unauthorized
    );
    assert_eq!(auth.refreshes, 1);
    // The users who waited are counted as having waited, apart from request latency:
    // that time is the generator blocked, not the service being slow.
    assert!(auth.blocked_us > 0, "{auth:?}");
    // And the run recovered: a refreshed token was retried, not reported.
    assert!(report.metrics.chains["ping"].completed > 0);
}

#[tokio::test]
async fn a_401_the_plan_calls_a_failure_is_counted_apart_from_application_errors() {
    let watched = Arc::new(Watched::default());
    // Nothing the plan can present is acceptable.
    watched.min_valid.store(u64::MAX, Ordering::SeqCst);
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "bearer", "token": "fixed-token",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "refresh": {"on_401": "fail"},
        }),
    );
    let report = run_bundle(dir.path()).await;

    let step = &report.metrics.chains["ping"].steps["get"];
    assert!(step.failed > 0);
    // Its own class. A 401 storm reported as an application error sends somebody
    // reading the service's code instead of checking the credential.
    assert_eq!(step.errors[10], step.failed, "unauthorized");
    assert_eq!(step.statuses[&401], step.attempted);
    // And nothing was refreshed, because the plan said not to.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ignoring_a_401_leaves_it_as_the_status_it_is() {
    let watched = Arc::new(Watched::default());
    // Nothing the plan can present is acceptable.
    watched.min_valid.store(u64::MAX, Ordering::SeqCst);
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "bearer", "token": "fixed-token",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "refresh": {"on_401": "ignore"},
        }),
    );
    let report = run_bundle(dir.path()).await;

    let step = &report.metrics.chains["ping"].steps["get"];
    // A chain that expects a 401 is a deliberately-failing flow, and the engine must
    // not decide on its own that a status the plan tolerates is a failure.
    assert_eq!(step.failed, 0);
    assert_eq!(step.statuses[&401], step.attempted);
}

#[tokio::test]
async fn basic_credentials_reach_the_service_as_the_header_it_expects() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "basic", "username": "aladdin", "password": "opensesame",
            "inject": {"header": "Authorization", "format": "Basic {{ token }}"},
        }),
    );
    run_bundle(dir.path()).await;

    let presented = watched.presented.lock().unwrap().clone();
    assert!(!presented.is_empty(), "nothing was sent");
    assert!(
        presented
            .iter()
            .all(|value| value == "Basic YWxhZGRpbjpvcGVuc2VzYW1l"),
        "{presented:?}"
    );
    // Nothing was fetched: a credential the plan already holds has nothing to fetch.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_login_request_is_a_call_like_any_other() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "login_request",
            "request": {
                "method": "POST", "path": "/login",
                "body": "{\"user\":\"ada\"}",
            },
            "extract": {"token": {"json": "$.token"}},
            "inject": {"header": "X-Session-Token", "format": "{{ token }}"},
        }),
    );
    run_bundle(dir.path()).await;

    let presented = watched.presented.lock().unwrap().clone();
    assert!(!presented.is_empty(), "nothing was sent");
    assert!(
        presented.iter().all(|value| value == "t-1"),
        "{presented:?}"
    );
    let body = watched.grants.lock().unwrap()[0].clone();
    assert!(body.contains("ada"), "{body}");
    // Once, before the run: a login is not load, and it does not spend a connection
    // the load was given either.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_token_per_row_of_a_dataset_is_pre_warmed_for_every_row() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{address}/token"),
            "client_id": "shop", "client_secret": "hunter2",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "identity": "from_dataset:tenants",
        }),
    );
    fs::create_dir_all(dir.path().join("data")).unwrap();
    fs::write(dir.path().join("data/tenants.csv"), "id\na\nb\nc\n").unwrap();
    edit(dir.path(), "mix.json", |doc| {
        doc["datasets"] = json!({"tenants": {"file": "data/tenants.csv", "mode": "round_robin"}});
    });
    let report = run_bundle(dir.path()).await;

    // Three rows, three tokens, all fetched before the clock started.
    assert_eq!(watched.issued.load(Ordering::SeqCst), 3);
    let auth = report.last_window.as_ref().unwrap().auth.unwrap();
    assert_eq!(auth.identities, 3);
    assert_eq!(auth.acquisitions, 3);
    // And each request carried the token of the row it was for: the token and the
    // request it signs have to be the same tenant.
    let presented: std::collections::BTreeSet<_> =
        watched.presented.lock().unwrap().iter().cloned().collect();
    assert_eq!(presented.len(), 3, "{presented:?}");
}

#[tokio::test]
async fn a_plan_whose_identity_provider_is_unreachable_stops_before_it_sends_load() {
    let watched = Arc::new(Watched::default());
    let address = serve(Arc::clone(&watched)).await;
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        address,
        json!({
            "mode": "oauth_client_credentials",
            "token_url": "http://127.0.0.1:1/token",
            "client_id": "shop", "client_secret": "hunter2",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
        }),
    );
    let error = run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .map(drop)
        .unwrap_err();
    // Before the arrival clock: a run that sent load it could not authenticate would
    // have measured a wall of 401s and called it a result.
    assert!(error.contains("auth"), "{error}");
    assert!(watched.presented.lock().unwrap().is_empty());
}

#[test]
fn a_format_that_reads_anything_but_the_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({
            "mode": "bearer", "token": "x",
            "inject": {"header": "Authorization", "format": "Bearer {{ order_id }}"},
        }),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("order_id"), "{error}");
}

#[test]
fn an_injected_header_the_transport_owns_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({
            "mode": "bearer", "token": "x",
            "inject": {"header": "Host", "format": "{{ token }}"},
        }),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("transport"), "{error}");
}

#[test]
fn an_identity_naming_a_dataset_nobody_declared_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    with_auth(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({
            "mode": "bearer", "token": "x",
            "inject": {"header": "Authorization", "format": "{{ token }}"},
            "identity": "from_dataset:tenants",
        }),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("tenants"), "{error}");
}
