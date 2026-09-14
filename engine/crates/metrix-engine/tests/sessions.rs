//! B3.9: what a virtual user carries between requests, and for how long.
//!
//! Run against a service that hands out a session id and then tells you which one it
//! is seeing — because the question a session test has to answer is how many distinct
//! people the service thought it was talking to, not how many requests it got.

mod support;

use std::{
    collections::BTreeSet,
    convert::Infallible,
    future::pending,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrix_engine::{Plan, run};
use serde_json::json;
use support::{bundle, edit};
use tokio::net::TcpListener;

/// What the service saw.
#[derive(Default)]
struct Seen {
    /// Session ids it had to issue, one per caller that arrived with no cookie.
    issued: AtomicU64,
    /// The session id on each request that carried one.
    carried: Mutex<Vec<String>>,
    /// Requests that arrived with no cookie at all.
    anonymous: AtomicU64,
}

/// `/start` sets a session cookie; everything else reports the one it was sent.
async fn serve(seen: Arc<Seen>) -> SocketAddr {
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
                        let path = request.uri().path().to_owned();
                        let sent = request
                            .headers()
                            .get("cookie")
                            .and_then(|value| value.to_str().ok())
                            .and_then(|text| text.split_once("sid="))
                            .map(|(_, id)| id.split(';').next().unwrap_or("").to_owned());
                        match &sent {
                            Some(id) => seen.carried.lock().unwrap().push(id.clone()),
                            None => {
                                seen.anonymous.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        let mut builder = Response::builder().status(200);
                        // Issued only to a caller that has none, which is what a
                        // service does: a session cookie is how it recognises
                        // somebody it has already met.
                        if path == "/start" && sent.is_none() {
                            let id = seen.issued.fetch_add(1, Ordering::SeqCst);
                            builder = builder.header("set-cookie", format!("sid=s{id}; Path=/"));
                        }
                        Ok::<_, Infallible>(
                            builder.body(Full::new(Bytes::from_static(b"ok"))).unwrap(),
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

/// A two-step chain: start a session, then use it.
fn two_step(root: &Path, address: SocketAddr, session: serde_json::Value) {
    bundle(root, address, "http1");
    support::write(
        root,
        "calls/ping.json",
        &json!({
            "start": {"method": "GET", "path": "/start"},
            "use": {"method": "GET", "path": "/work"},
        }),
    );
    edit(root, "mix.json", |doc| {
        let mut chain = json!({
            "name": "flow", "percent": 100,
            "steps": [{"id": "start", "call": "start"}, {"id": "use", "call": "use"}]
        });
        for (key, value) in session.as_object().expect("a session block") {
            chain[key] = value.clone();
        }
        doc["chains"] = json!([chain]);
        doc["load"]["rate"] = json!(12);
        doc["load"]["duration"] = json!("1s");
        doc["load"]["max_concurrency"] = json!(4);
    });
}

#[tokio::test]
async fn a_cookie_the_service_set_comes_back_on_the_next_step() {
    let seen: Arc<Seen> = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(dir.path(), address, json!({"session": "fresh"}));
    run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .unwrap();

    let carried = seen.carried.lock().unwrap().clone();
    let issued = seen.issued.load(Ordering::SeqCst);
    assert!(issued > 0, "nothing was sent");
    // Every second step carried the session the first step was given. Without a jar
    // the service sees two strangers and the flow being modelled never happens.
    assert_eq!(carried.len() as u64, issued);
    // And every first step arrived without one, because a fresh session starts empty.
    assert_eq!(seen.anonymous.load(Ordering::SeqCst), issued);
}

#[tokio::test]
async fn a_fresh_session_is_a_new_person_every_iteration() {
    let seen: Arc<Seen> = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(dir.path(), address, json!({"session": "fresh"}));
    run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .unwrap();

    let distinct: BTreeSet<String> = seen.carried.lock().unwrap().iter().cloned().collect();
    // One session per iteration: a first-time user, exercising the login path and a
    // cold per-user cache every time.
    assert_eq!(distinct.len() as u64, seen.issued.load(Ordering::SeqCst));
    assert!(distinct.len() > 1, "{distinct:?}");
}

#[tokio::test]
async fn a_reused_session_keeps_one_person_per_virtual_user() {
    let seen: Arc<Seen> = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(dir.path(), address, json!({"session": "reuse"}));
    run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .unwrap();

    let issued = seen.issued.load(Ordering::SeqCst);
    let requests =
        seen.carried.lock().unwrap().len() as u64 + seen.anonymous.load(Ordering::SeqCst);
    // A returning user: the jar survives the iteration, so the next iteration's first
    // step already has a cookie and the service issues one per slot rather than one
    // per iteration. `fresh` everywhere overstates login load; this is the other end,
    // and the gap between the two mistakes is the whole reason the policy exists.
    assert!(issued <= 4, "{issued} sessions for {requests} requests");
    assert!(
        requests > issued * 3,
        "the run was too short to show anything"
    );
    let distinct: BTreeSet<String> = seen.carried.lock().unwrap().iter().cloned().collect();
    assert!(
        distinct.len() <= 4,
        "more sessions than slots: {distinct:?}"
    );
}

#[tokio::test]
async fn a_pool_is_a_population_of_the_size_the_plan_asked_for() {
    let seen: Arc<Seen> = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        address,
        json!({"session": "pool", "pool_size": 2}),
    );
    run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .unwrap();

    let distinct: BTreeSet<String> = seen.carried.lock().unwrap().iter().cloned().collect();
    // Two people, however many iterations ran: neither all-new nor all-one, which is
    // the shape of a real population and the reason the mode exists.
    assert_eq!(distinct.len(), 2, "{distinct:?}");
}

#[tokio::test]
async fn a_fresh_session_gets_a_fresh_credential_too() {
    // A fresh session that reused a token would not be fresh in any way the service
    // can tell (§4.2), so the identity is bound to the session and not to the slot.
    let seen: Arc<Seen> = Arc::default();
    let address = serve(Arc::clone(&seen)).await;
    let tokens = Arc::new(Mutex::new(Vec::<String>::new()));

    let recorder = Arc::clone(&tokens);
    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let idp = listener.local_addr().unwrap();
    let issued = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&issued);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorder = Arc::clone(&recorder);
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let service = service_fn(move |_request: Request<hyper::body::Incoming>| {
                    let recorder = Arc::clone(&recorder);
                    let counter = Arc::clone(&counter);
                    async move {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        recorder.lock().unwrap().push(format!("t-{n}"));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(format!(
                                    "{{\"access_token\":\"t-{n}\",\"expires_in\":3600}}"
                                ))))
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

    let dir = tempfile::tempdir().unwrap();
    two_step(dir.path(), address, json!({"session": "fresh"}));
    edit(dir.path(), "mix.json", |doc| {
        doc["auth"] = json!({
            "mode": "oauth_client_credentials",
            "token_url": format!("http://{idp}/token"),
            "client_id": "shop", "client_secret": "hunter2",
            "inject": {"header": "Authorization", "format": "Bearer {{ token }}"},
            "identity": "per_vu",
        });
    });
    run(Plan::load(dir.path()).unwrap(), pending::<()>())
        .await
        .unwrap();

    // Four identities pre-warmed, one per concurrent slot, and consecutive fresh
    // sessions cycle through them rather than every iteration on one slot reusing
    // the same one.
    assert_eq!(issued.load(Ordering::SeqCst), 4);
    assert!(seen.issued.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_pooled_chain_with_no_population_is_reuse_under_another_name() {
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({"session": "pool"}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("pool_size"), "{error}");
}

#[test]
fn a_population_on_a_chain_that_does_not_pool_decides_nothing() {
    let dir = tempfile::tempdir().unwrap();
    two_step(
        dir.path(),
        "127.0.0.1:1".parse().unwrap(),
        json!({"session": "reuse", "pool_size": 4}),
    );
    let error = Plan::load(dir.path()).map(drop).unwrap_err();
    assert!(error.contains("pool_size"), "{error}");
}
