//! B3's own acceptance: the shape of `examples/plans/checkout-mixed`, running.
//!
//! Six chains at declared percentages, XML and JSON, extraction between steps, a Lua
//! generator, OAuth with refresh, and a deliberately-failing chain whose 401s count as
//! passes. Every one of those is tested on its own elsewhere; what this asks is
//! whether they hold together in one plan, which is a different question and the one
//! the milestone is about.
//!
//! The example bundle itself points at two ECS tasks with a `host_header` and a
//! `shuffle` order — all B4 — so the bundle here is that plan's shape aimed at
//! servers a test can start.

mod support;

use std::{
    collections::BTreeMap,
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
use serde_json::json;
use support::{bundle, edit};
use tokio::net::TcpListener;

/// What the shop was asked for, so the test can say which flows actually happened.
#[derive(Default)]
struct Shop {
    /// Requests per path prefix.
    paths: Mutex<BTreeMap<String, u64>>,
    /// Order bodies, to prove the generator's XML arrived.
    orders: Mutex<Vec<String>>,
    /// Credentials presented, to prove auth reached every chain.
    credentials: Mutex<Vec<String>>,
    /// Tokens issued by the identity provider.
    issued: AtomicU64,
    /// Revoke every token once this many orders have been posted, so the run has to
    /// refresh part-way through.
    revoke_after: u64,
    min_valid: AtomicU64,
}

async fn serve(shop: Arc<Shop>) -> SocketAddr {
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let shop = Arc::clone(&shop);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let shop = Arc::clone(&shop);
                    async move { Ok::<_, Infallible>(answer(shop, request).await) }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

async fn answer(shop: Arc<Shop>, request: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let credential = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
        .unwrap_or_default();

    if path == "/oauth2/token" {
        let issued = shop.issued.fetch_add(1, Ordering::SeqCst) + 1;
        return json_body(
            200,
            &format!("{{\"access_token\":\"t-{issued}\",\"expires_in\":3600}}"),
        );
    }

    *shop.paths.lock().unwrap().entry(bucket(&path)).or_default() += 1;
    if !credential.is_empty() {
        shop.credentials.lock().unwrap().push(credential.clone());
    }

    // Every path but the token endpoint is behind the credential, which is what makes
    // the refresh happen where the plan says it does.
    let generation = credential
        .rsplit_once("t-")
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .unwrap_or(0);
    if generation < shop.min_valid.load(Ordering::SeqCst) {
        return Response::builder()
            .status(401)
            .body(Full::new(Bytes::from_static(b"stale token")))
            .unwrap();
    }

    match (method.as_str(), path.as_str()) {
        // The deliberately-failing chain: the password is wrong and 401 is the point.
        ("POST", "/api/session") if body.contains("not-the-password") => Response::builder()
            .status(401)
            .body(Full::new(Bytes::from_static(b"no")))
            .unwrap(),
        ("POST", "/api/session") => json_body(200, r#"{"token":"sess-1","user":"ada"}"#),
        ("DELETE", "/api/session") => Response::builder()
            .status(204)
            .body(Full::new(Bytes::new()))
            .unwrap(),
        ("GET", "/api/products/search") => json_body(
            200,
            r#"{"items":[{"id":"P-77","name":"shoes"},{"id":"P-78","name":"hat"}]}"#,
        ),
        ("POST", "/api/cart/items") => json_body(201, r#"{"id":"L-5","qty":1}"#),
        ("DELETE", path) if path.starts_with("/api/cart/items/") => {
            // Only for the line the add step issued: a chain that substituted nothing
            // would land on `/api/cart/items/` and this is where that shows up.
            let status = if path == "/api/cart/items/L-5" {
                204
            } else {
                404
            };
            Response::builder()
                .status(status)
                .body(Full::new(Bytes::new()))
                .unwrap()
        }
        ("POST", "/api/orders") => {
            let count = {
                let mut orders = shop.orders.lock().unwrap();
                orders.push(body);
                orders.len() as u64
            };
            if shop.revoke_after > 0 && count == shop.revoke_after {
                shop.min_valid
                    .store(shop.issued.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            }
            xml_body(201, "<order><id>O-900</id></order>")
        }
        ("GET", path) if path.starts_with("/api/orders/") => {
            // The chain polls this until it says complete, so it has to say complete.
            if path == "/api/orders/O-900" {
                json_body(200, r#"{"status":"complete"}"#)
            } else {
                Response::builder()
                    .status(404)
                    .body(Full::new(Bytes::new()))
                    .unwrap()
            }
        }
        _ => Response::builder()
            .status(404)
            .body(Full::new(Bytes::new()))
            .unwrap(),
    }
}

fn bucket(path: &str) -> String {
    match path {
        p if p.starts_with("/api/cart/items/") => "/api/cart/items/{id}".into(),
        p if p.starts_with("/api/orders/") => "/api/orders/{id}".into(),
        other => other.to_owned(),
    }
}

fn json_body(status: u16, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap()
}

fn xml_body(status: u16, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/xml")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap()
}

/// The example's plan, aimed at one address a test can start.
fn checkout_mixed(root: &Path, address: SocketAddr) {
    bundle(root, address, "http1");
    fs::create_dir_all(root.join("gen")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../examples/plans/checkout-mixed")
        .canonicalize()
        .expect("the example bundle");
    // The example's own generator and dataset, so this runs what is committed rather
    // than a copy that can drift from it.
    fs::copy(repo.join("gen/order.lua"), root.join("gen/order.lua")).unwrap();
    fs::copy(repo.join("data/users.csv"), root.join("data/users.csv")).unwrap();
    fs::copy(repo.join("calls/shop.json"), root.join("calls/shop.json")).unwrap();
    fs::remove_file(root.join("calls/ping.json")).unwrap();

    let example: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.join("mix.json")).unwrap()).unwrap();
    edit(root, "mix.json", |doc| {
        for field in ["datasets", "generators", "chains", "capture", "defaults"] {
            doc[field] = example[field].clone();
        }
        doc["calls"] = json!(["calls/shop.json"]);
        doc["auth"] = json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{address}/oauth2/token"),
            "client_id": "shop", "client_secret": "hunter2", "scope": "orders.write",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "refresh": {"strategy": "expires_in_margin", "margin_s": 30, "on_401": "refresh_once"},
            "identity": "shared",
        });
        doc["load"]["rate"] = json!(120);
        doc["load"]["duration"] = json!("2s");
        doc["load"]["max_concurrency"] = json!(40);
        doc["engine"]["connections_per_host"] = json!(40);
    });
}

async fn run_it(shop: Arc<Shop>) -> (Report, Arc<Shop>) {
    let address = serve(Arc::clone(&shop)).await;
    let dir = tempfile::tempdir().unwrap();
    checkout_mixed(dir.path(), address);
    let plan = Plan::load(dir.path()).unwrap();
    let report = run(plan, pending::<()>()).await.unwrap();
    (report, shop)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_whole_mixture_runs_and_every_part_of_it_did_what_it_said() {
    let (report, shop) = run_it(Arc::new(Shop {
        // Part-way through, so the run has to refresh mid-flight rather than at rest.
        revoke_after: 5,
        ..Default::default()
    }))
    .await;

    // --- six chains, at the percentages they declared -----------------------------
    let chains = &report.metrics.chains;
    assert_eq!(chains.len(), 6, "{:?}", chains.keys().collect::<Vec<_>>());
    let started: u64 = chains.values().map(|chain| chain.started).sum();
    assert_eq!(started, report.admitted);
    for (name, share) in [
        ("login-fail", 30.0),
        ("login", 10.0),
        ("logout", 5.0),
        ("search", 20.0),
        ("cart-add-remove", 20.0),
        ("checkout", 15.0),
    ] {
        let ran = chains[name].started as f64 / started as f64 * 100.0;
        // Within a point, because the mixture is exact at every prefix rather than
        // approached over the run: a percentage is a claim about what the service was
        // asked for.
        assert!(
            (ran - share).abs() < 1.0,
            "{name} ran at {ran:.1}% of {started}, not {share}%"
        );
    }

    // --- a deliberately-failing chain whose 401s are passes ------------------------
    let bad = &chains["login-fail"].steps["post"];
    assert_eq!(bad.statuses[&401], bad.attempted);
    assert_eq!(bad.failed, 0, "an expected 401 was counted as an error");
    assert_eq!(chains["login-fail"].aborted, 0);

    // --- extraction between steps, JSON and XML ------------------------------------
    let paths = shop.paths.lock().unwrap().clone();
    // The cart chain read back the line id the add step returned, and the checkout
    // chain read back the order id it parsed out of XML. A step that substituted
    // nothing would have landed on `/api/cart/items/` or `/api/orders/` and been
    // answered 404, which is the signal this is looking for — a 401 in here is the
    // mid-run revocation being retried, and a 404 is extraction having failed.
    let remove = &chains["cart-add-remove"].steps["remove"];
    assert!(remove.statuses[&204] > 0);
    assert_eq!(remove.statuses.get(&404), None, "{:?}", remove.statuses);
    let poll = &chains["checkout"].steps["poll"];
    assert!(poll.statuses[&200] > 0);
    assert_eq!(poll.statuses.get(&404), None, "{:?}", poll.statuses);
    assert!(paths.contains_key("/api/cart/items/{id}"), "{paths:?}");

    // --- the Lua generator built the XML body --------------------------------------
    let orders = shop.orders.lock().unwrap().clone();
    assert!(!orders.is_empty(), "no orders were posted");
    for order in &orders {
        assert!(order.starts_with("<order><user>"), "{order}");
        // The dataset row reached it, and so did the product id the search step
        // extracted -- through `ctx.args`, `ctx.rows` and `ctx.vars` respectively.
        assert!(order.contains("@example.test"), "{order}");
        assert!(order.contains("<line><sku>SKU-"), "{order}");
    }
    let generation = &report.metrics.generation["order-xml"];
    assert_eq!(generation.failed, 0);
    assert_eq!(
        generation.calls,
        chains["checkout"].steps["create"].attempted
    );
    assert_eq!(chains["checkout"].aborted, 0);

    // --- OAuth, with a refresh, and none of it in the load figures ------------------
    let auth = report.last_window.as_ref().unwrap().auth.unwrap();
    assert!(auth.refreshes >= 1, "the revocation never forced a refresh");
    assert_eq!(auth.failures, 0);
    let credentials: std::collections::BTreeSet<_> =
        shop.credentials.lock().unwrap().iter().cloned().collect();
    assert!(
        credentials.len() >= 2,
        "the token never changed: {credentials:?}"
    );
    // Token traffic went to the same box and is still not load: the shop counted
    // every request the run made, the engine's own per-step counts add up to the same
    // number, and the token endpoint is in neither.
    let requests: u64 = paths.values().sum();
    let attempted: u64 = chains
        .values()
        .flat_map(|chain| chain.steps.values())
        .map(|step| step.attempted)
        .sum();
    assert!(!paths.contains_key("/oauth2/token"), "{paths:?}");
    assert_eq!(requests, attempted, "{paths:?}");

    // --- and the run as a whole is sound -------------------------------------------
    assert_eq!(report.timed_out, 0);
    assert_eq!(report.skipped_concurrency, 0);
    assert!(report.metrics.chain.count() > 0);
}
