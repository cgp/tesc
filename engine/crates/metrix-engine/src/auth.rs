//! Getting a credential and keeping it fresh, without that becoming the measurement.
//!
//! A first-class block rather than a hand-rolled chain step (design-engine §6):
//! acquiring a token is infrastructure for the test, not the thing being measured,
//! and conflating the two corrupts both the throughput figure and the latency
//! distribution. Four rules follow, and every one is here for a number that would
//! otherwise be wrong:
//!
//! - **Auth traffic is excluded from load metrics.** Token calls are not counted in
//!   RPS, not mixed into the latency histogram, and not counted in the error rate.
//!   They go out over their own pool and are reported on their own lines. Without
//!   this, pointing a test at an IdP-protected service silently inflates the
//!   throughput figure with calls to a different system — and even a login against
//!   the target itself would be spending the connections the load was given.
//! - **Tokens are acquired before the measured window**, never first-touched inside
//!   it, and the pool is pre-warmed to the size `identity` implies.
//! - **Refresh is single-flight.** Two hundred virtual users hitting a 401 at the
//!   same instant send exactly one refresh and the rest wait on it. The naive version
//!   sends two hundred, floods the IdP, and produces a spike that reads as the target
//!   degrading. Time spent blocked is measured and reported apart from latency.
//! - **Secrets never live in the plan.** `{{ env.X }}` and `{{ secret.X }}` are
//!   resolved from the process environment when the plan compiles (see `template`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hyper::{
    HeaderMap, Method, Uri,
    header::{HeaderName, HeaderValue},
};
use metrix_plan::{
    Auth as Declared, AuthMode, Identity as Declared_identity, On401, Refresh, RefreshStrategy,
    Selector, Target,
};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::calls::{Context, Prepared, RequestTemplate};
use crate::dataset::Datasets;
use crate::extract::{Extractor, Response};
use crate::http::{Pool, SendState, Timing, send};
use crate::template::{Scope, Template, Unbound, Values};

/// How long a token is assumed good for when the service does not say.
///
/// Short on purpose: guessing long means the first anyone hears of the guess being
/// wrong is a 401 storm partway through the measured window.
const ASSUMED_LIFETIME: Duration = Duration::from_secs(300);

/// A token request that takes longer than this is a broken identity provider, and
/// waiting on it means the run never starts.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// What one identity currently holds.
#[derive(Clone, Default)]
struct Held {
    value: String,
    /// When it stops being usable, already reduced by the configured margin.
    good_until: Option<Instant>,
}

/// One identity's token, and the guard that keeps its refreshes single-flight.
#[derive(Default)]
struct Slot {
    held: RwLock<Held>,
    /// Held by whichever caller is refreshing. The others wait here rather than each
    /// sending their own, and re-read the token when they get in.
    refreshing: Mutex<()>,
}

/// Who the load is authenticated as (§6.2).
enum Identity {
    /// One token for all virtual users. Cheapest, and hides per-user rate limiting
    /// and per-user cache locality entirely.
    Shared,
    /// One per virtual user.
    PerVu,
    /// One per row of a dataset, taken by the same rule the row itself is: the token
    /// and the request it signs have to be the same tenant.
    FromDataset(usize),
}

/// How a credential is obtained.
enum Mode {
    /// A credential the plan already holds: nothing to fetch, nothing to refresh.
    Fixed(String),
    /// A form POST to a token endpoint, per RFC 6749.
    Form {
        path: String,
        body: String,
        token: Extractor,
        lifetime: Extractor,
    },
    /// A bespoke login, reusing the request and extraction machinery of §5.
    Login {
        request: Arc<RequestTemplate>,
        token: Extractor,
        expires_at: Option<Extractor>,
    },
}

/// Counted apart from the run's own metrics, because that is the whole point (§6.3).
#[derive(Default)]
pub(crate) struct Counters {
    pub acquisitions: AtomicU64,
    pub refreshes: AtomicU64,
    pub failures: AtomicU64,
    /// 401s from the target that caused a refresh, rather than every 401 seen: a
    /// chain that asserts 401 is not an auth problem.
    pub unauthorized: AtomicU64,
    pub refresh_us: AtomicU64,
    /// Virtual-user time spent waiting on somebody else's refresh.
    pub blocked_us: AtomicU64,
}

/// The `auth` block, compiled.
pub(crate) struct Auth {
    header: HeaderName,
    /// `"Bearer {{ token }}"`, with `token` the only variable it may read.
    format: Template,
    mode: Mode,
    refresh: Refresh,
    identity: Identity,
    slots: Vec<Slot>,
    /// Where auth requests go: the identity provider, or the target itself for a
    /// login request. Its own pool either way, so a token fetch never spends a
    /// connection the load was given.
    target: Option<Target>,
    pool: Mutex<Option<Pool>>,
    pub counters: Counters,
}

impl Auth {
    /// Compile the block. Secrets are already literal by the time templates parse.
    pub fn compile(
        declared: &Declared,
        concurrency: usize,
        context: &Context<'_>,
    ) -> Result<Option<Self>, String> {
        let at = "mix.json/auth";
        if matches!(declared.mode, AuthMode::None) {
            return Ok(None);
        }
        let datasets = context.datasets;
        let header = HeaderName::from_bytes(declared.inject.header.as_bytes())
            .map_err(|_| format!("{at}/inject/header: not a header name"))?;
        require(
            !crate::calls::transport_managed(header.as_str()),
            &format!("{at}/inject/header: {header} is managed by the transport"),
        )?;
        let format = Template::parse(
            &format!("{at}/inject/format"),
            &declared.inject.format,
            datasets,
        )?;
        for name in format.variables() {
            require(
                name == "token",
                &format!(
                    "{at}/inject/format: {{{{ {name} }}}} is not available here; the only \
                     value an injected credential has is {{{{ token }}}}"
                ),
            )?;
        }

        let (mode, target) = compile_mode(at, declared, context)?;
        let identity = match &declared.identity {
            Declared_identity::Shared => Identity::Shared,
            Declared_identity::PerVu => Identity::PerVu,
            Declared_identity::FromDataset(reference) => {
                let name = reference.strip_prefix("from_dataset:").ok_or_else(|| {
                    format!(
                        "{at}/identity: expected shared, per_vu or from_dataset:<name>, got \
                         {reference:?}"
                    )
                })?;
                Identity::FromDataset(datasets.index(&format!("{at}/identity"), name)?)
            }
        };
        // Pre-warmed to the size `identity` implies: a token first fetched inside the
        // measured window is a token fetch inside the measured window.
        let count = match &identity {
            Identity::Shared => 1,
            Identity::PerVu => concurrency,
            Identity::FromDataset(index) => datasets.get(*index).rows(),
        };
        let mut slots = Vec::new();
        slots.resize_with(count.max(1), Slot::default);

        Ok(Some(Self {
            header,
            format,
            mode,
            refresh: declared.refresh.clone(),
            identity,
            slots,
            target,
            pool: Mutex::new(None),
            counters: Counters::default(),
        }))
    }

    /// Fill every slot before the arrival clock starts.
    pub async fn warm(&self) -> Result<(), String> {
        if let Some(target) = &self.target {
            // One connection of its own, opened before the clock: the first token
            // fetch must not pay for a handshake inside the measured window either.
            let pool = Pool::prepare(target, 1, TOKEN_TIMEOUT)
                .await
                .map_err(|error| {
                    format!("mix.json/auth: cannot reach {} — {error:?}", target.address)
                })?;
            *self.pool.lock().await = Some(pool);
        }
        for index in 0..self.slots.len() {
            self.acquire(index, None).await.map_err(|error| {
                format!("mix.json/auth: cannot get a token before the run starts — {error}")
            })?;
        }
        Ok(())
    }

    /// Which identity this iteration authenticates as.
    fn slot_for(&self, vu: usize, iteration: u64, seed: u64, datasets: &Datasets) -> usize {
        match &self.identity {
            Identity::Shared => 0,
            Identity::PerVu => vu % self.slots.len(),
            // The same rule the request's own row is taken by, so the token and the
            // request it signs are the same tenant.
            Identity::FromDataset(index) => datasets.get(*index).row(iteration, seed),
        }
    }

    /// Attach the credential to a request that is about to be sent.
    ///
    /// Refreshes first when the token is past its margin, so the renewal lands here
    /// rather than as a 401 in the measured window's error rate.
    /// Returns the credential it attached, so that if the target rejects this
    /// request the caller can say *which* token was rejected. Re-reading the current
    /// one instead would make a refresh that has already happened look like it has
    /// not, and every straggler from the old token would trigger another.
    pub async fn inject(&self, prepared: &mut Prepared, who: Who<'_>) -> Result<String, String> {
        let index = self.slot_for(who.vu, who.iteration, who.seed, who.datasets);
        let token = self.current(index).await?;
        let value = HeaderValue::from_str(&self.render(&token, who.iteration)?)
            .map_err(|_| "the credential does not fit in a header".to_owned())?;
        prepared.headers.insert(self.header.clone(), value);
        Ok(token)
    }

    fn render(&self, token: &str, iteration: u64) -> Result<String, String> {
        let mut scope = Scope::new();
        scope.insert("token".to_owned(), token.to_owned());
        let empty = Datasets::default();
        let mut values = Values::new(&scope, &empty, 0, iteration);
        self.format
            .render(&mut values)
            .map_err(|Unbound(what)| what)
    }

    /// What to do about a 401 the target returned.
    ///
    /// `refresh_once` is the default because a token that expired mid-run is the
    /// ordinary case, and re-sending after one refresh is what a client would do.
    pub async fn on_unauthorized(&self, who: Who<'_>, rejected: &str) -> Retry {
        if self.refresh.on_401 != On401::RefreshOnce {
            return Retry::No;
        }
        self.counters.unauthorized.fetch_add(1, Ordering::Relaxed);
        let index = self.slot_for(who.vu, who.iteration, who.seed, who.datasets);
        // Forced, with the value the target actually rejected: that token may be
        // nowhere near its stated expiry, and the service is the authority on whether
        // it works. Passing what was rejected rather than what is current is what
        // makes a burst of stragglers cost nothing after the first refresh.
        match self.acquire(index, Some(rejected.to_owned())).await {
            Ok(()) => Retry::Yes,
            Err(_) => Retry::No,
        }
    }

    /// What auth has cost so far, for the window being written (§6.3).
    pub fn snapshot(&self) -> metrix_metrics::aggregation::AuthCounts {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        metrix_metrics::aggregation::AuthCounts {
            identities: self.slots.len() as u64,
            acquisitions: read(&self.counters.acquisitions),
            refreshes: read(&self.counters.refreshes),
            failures: read(&self.counters.failures),
            unauthorized: read(&self.counters.unauthorized),
            refresh_us: read(&self.counters.refresh_us),
            blocked_us: read(&self.counters.blocked_us),
        }
    }

    /// True when the plan says a 401 is a failure rather than a renewal.
    pub fn fails_on_401(&self) -> bool {
        self.refresh.on_401 == On401::Fail
    }

    async fn held_value(&self, index: usize) -> String {
        self.slots[index].held.read().await.value.clone()
    }

    /// The token to send, refreshing first if this one is past its margin.
    async fn current(&self, index: usize) -> Result<String, String> {
        {
            let held = self.slots[index].held.read().await;
            if !held.value.is_empty() && !expiring(&held, self.refresh.strategy) {
                return Ok(held.value.clone());
            }
        }
        let stale = self.held_value(index).await;
        self.acquire(index, Some(stale)).await?;
        Ok(self.held_value(index).await)
    }

    /// Get a token for this slot, once, however many callers ask at the same time.
    ///
    /// `stale` is the value the caller found unusable. Whoever takes the lock second
    /// finds a different value already there and uses it, which is the difference
    /// between one refresh and two hundred.
    async fn acquire(&self, index: usize, stale: Option<String>) -> Result<(), String> {
        let waited = Instant::now();
        let _guard = self.slots[index].refreshing.lock().await;
        let blocked = waited.elapsed();
        if !blocked.is_zero() {
            self.counters
                .blocked_us
                .fetch_add(blocked.as_micros() as u64, Ordering::Relaxed);
        }
        if let Some(stale) = &stale {
            let held = self.slots[index].held.read().await;
            if !held.value.is_empty() && held.value != *stale {
                return Ok(());
            }
        }

        let first = stale.is_none();
        let started = Instant::now();
        let fetched = self.fetch(index).await;
        self.counters
            .refresh_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        match fetched {
            Ok(held) => {
                if first {
                    self.counters.acquisitions.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.counters.refreshes.fetch_add(1, Ordering::Relaxed);
                }
                *self.slots[index].held.write().await = held;
                Ok(())
            }
            Err(error) => {
                self.counters.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    /// One round trip to whatever issues the credential.
    async fn fetch(&self, index: usize) -> Result<Held, String> {
        let margin = Duration::from_secs(self.refresh.margin_s);
        match &self.mode {
            Mode::Fixed(value) => Ok(Held {
                value: value.clone(),
                good_until: None,
            }),
            Mode::Form {
                path,
                body,
                token,
                lifetime,
            } => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    hyper::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
                let (got, captured) = self
                    .round_trip(Method::POST, path, headers, Bytes::from(body.clone()))
                    .await?;
                let document = Response::new(&got, &captured);
                let value = document
                    .read(token)
                    .ok_or_else(|| "the token endpoint returned no access_token".to_owned())?;
                let lived = document
                    .read(lifetime)
                    .and_then(|seconds| seconds.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(ASSUMED_LIFETIME);
                Ok(Held {
                    value,
                    good_until: Some(Instant::now() + lived.saturating_sub(margin)),
                })
            }
            Mode::Login {
                request,
                token,
                expires_at,
            } => {
                // Rendered against the identity's own index, so a per-row login sends
                // that row's credentials.
                let scope = Scope::new();
                let empty = Datasets::default();
                let mut values = Values::new(&scope, &empty, 0, index as u64);
                let prepared = request.render(&mut values).map_err(|Unbound(what)| what)?;
                let path = prepared
                    .uri
                    .path_and_query()
                    .map(|part| part.to_string())
                    .unwrap_or_else(|| "/".to_owned());
                let (got, captured) = self
                    .round_trip(
                        request.method.clone(),
                        &path,
                        prepared.headers.clone(),
                        prepared.body.clone(),
                    )
                    .await?;
                let document = Response::new(&got, &captured);
                let value = document
                    .read(token)
                    .ok_or_else(|| "the login response carried no token".to_owned())?;
                let lived = expires_at
                    .as_ref()
                    .and_then(|selector| document.read(selector))
                    .and_then(|text| text.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(ASSUMED_LIFETIME);
                Ok(Held {
                    value,
                    good_until: Some(Instant::now() + lived.saturating_sub(margin)),
                })
            }
        }
    }

    /// Send one auth request, on the auth pool, counted nowhere else.
    async fn round_trip(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(HeaderMap, Bytes), String> {
        let mut guard = self.pool.lock().await;
        let pool = guard
            .as_mut()
            .ok_or_else(|| "the auth pool was never opened".to_owned())?;
        let endpoint = Arc::clone(&pool.endpoint);
        let mut lease = pool
            .acquire()
            .ok_or_else(|| "no connection to the auth endpoint".to_owned())?;
        let template = RequestTemplate::fixed(
            method,
            path.parse::<Uri>()
                .map_err(|_| format!("{path:?} is not a path"))?,
            headers,
            body,
            TOKEN_TIMEOUT,
        );
        let now = Instant::now();
        let observation = send(
            &mut lease,
            &endpoint,
            &template,
            None,
            Timing {
                scheduled: now,
                admitted: now,
                records_drift: false,
            },
            &SendState::default(),
        )
        .await;
        pool.release(lease);
        if let Some(failure) = observation.error {
            return Err(format!("{failure:?}"));
        }
        let status = observation.status.unwrap_or(0);
        require(
            (200..300).contains(&status),
            &format!("the auth endpoint answered {status}"),
        )?;
        let captured = observation
            .response
            .ok_or_else(|| "the auth endpoint returned no body".to_owned())?;
        Ok((captured.headers, captured.body))
    }
}

/// Which virtual user, on which iteration, is asking for a credential.
#[derive(Clone, Copy)]
pub(crate) struct Who<'a> {
    pub vu: usize,
    pub iteration: u64,
    pub seed: u64,
    pub datasets: &'a Datasets,
}

/// Whether to send this request again once the credential has been renewed.
#[derive(PartialEq, Eq)]
pub(crate) enum Retry {
    Yes,
    No,
}

fn expiring(held: &Held, strategy: RefreshStrategy) -> bool {
    match strategy {
        // The plan says this credential does not go stale; believe it rather than
        // sending refreshes nobody asked for.
        RefreshStrategy::Never => false,
        RefreshStrategy::ExpiresInMargin => held
            .good_until
            .is_some_and(|deadline| Instant::now() >= deadline),
    }
}

fn compile_mode(
    at: &str,
    declared: &Declared,
    context: &Context<'_>,
) -> Result<(Mode, Option<Target>), String> {
    Ok(match &declared.mode {
        AuthMode::None => unreachable!("the caller returns early"),
        AuthMode::Basic { username, password } => (
            Mode::Fixed(base64(format!("{username}:{password}").as_bytes())),
            None,
        ),
        AuthMode::Bearer { token } => (Mode::Fixed(token.clone()), None),
        AuthMode::OauthClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
        } => {
            let mut body = format!(
                "grant_type=client_credentials&client_id={}&client_secret={}",
                crate::template::encode(client_id),
                crate::template::encode(client_secret)
            );
            append_scope(&mut body, scope);
            let (path, target) = token_target(at, token_url)?;
            (form_mode(at, path, body)?, Some(target))
        }
        AuthMode::OauthPassword {
            token_url,
            client_id,
            username,
            password,
            scope,
        } => {
            let mut body = format!(
                "grant_type=password&client_id={}&username={}&password={}",
                crate::template::encode(client_id),
                crate::template::encode(username),
                crate::template::encode(password)
            );
            append_scope(&mut body, scope);
            let (path, target) = token_target(at, token_url)?;
            (form_mode(at, path, body)?, Some(target))
        }
        AuthMode::LoginRequest { request, extract } => {
            let at = format!("{at}/mode");
            let compiled =
                crate::calls::compile("auth login", request, crate::calls::Reads::all(), context)?;
            let token = extract
                .get("token")
                .ok_or_else(|| {
                    format!("{at}/extract: a login request must extract {{{{ token }}}}")
                })
                .and_then(|selector| {
                    Extractor::compile(&format!("{at}/extract/token"), selector)
                })?;
            let expires_at = extract
                .get("expires_at")
                .map(|selector| Extractor::compile(&format!("{at}/extract/expires_at"), selector))
                .transpose()?;
            (
                Mode::Login {
                    request: Arc::new(compiled),
                    token,
                    expires_at,
                },
                // The target itself, and still its own pool: a login is not load, and
                // it must not spend a connection the load was given.
                Some(context.target.clone()),
            )
        }
    })
}

fn append_scope(body: &mut String, scope: &Option<String>) {
    if let Some(scope) = scope {
        body.push('&');
        body.push_str("scope=");
        body.push_str(&crate::template::encode(scope));
    }
}

fn form_mode(at: &str, path: String, body: String) -> Result<Mode, String> {
    Ok(Mode::Form {
        path,
        body,
        token: Extractor::compile(
            &format!("{at}/token_url"),
            &Selector::Json("$.access_token".to_owned()),
        )?,
        lifetime: Extractor::compile(
            &format!("{at}/token_url"),
            &Selector::Json("$.expires_in".to_owned()),
        )?,
    })
}

/// A token URL, split into the path its requests ask for and the box they go to.
fn token_target(at: &str, url: &str) -> Result<(String, Target), String> {
    let uri: Uri = url
        .parse()
        .map_err(|_| format!("{at}/token_url: {url:?} is not a URL"))?;
    let scheme = uri.scheme_str().unwrap_or("https");
    require(
        matches!(scheme, "http" | "https"),
        &format!("{at}/token_url: {scheme:?} is not a scheme the engine speaks"),
    )?;
    let host = uri
        .host()
        .ok_or_else(|| format!("{at}/token_url: {url:?} names no host"))?;
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "http" { 80 } else { 443 });
    let path = uri
        .path_and_query()
        .map(|part| part.to_string())
        .unwrap_or_else(|| "/".to_owned());
    Ok((
        path,
        Target {
            id: "auth".to_owned(),
            address: format!("{host}:{port}"),
            http_version: metrix_plan::HttpVersion::Auto,
            // The IdP is reached by name, so it verifies and vhosts on that name.
            host_header: None,
            tls: metrix_plan::Tls {
                enabled: scheme == "https",
                ..Default::default()
            },
            attributes: Default::default(),
        },
    ))
}

/// Base64 without a dependency: the alphabet is fixed and this runs once per plan.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((packed >> (18 - index * 6)) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_credentials_are_encoded_the_way_the_header_expects() {
        // The one case where the engine builds a credential itself, so it has to
        // match what every server decodes.
        assert_eq!(base64(b"aladdin:opensesame"), "YWxhZGRpbjpvcGVuc2VzYW1l");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn a_token_url_becomes_a_path_and_a_box_to_ask() {
        let (path, target) =
            token_target("at", "https://idp.example.com/oauth2/token?v=2").unwrap();
        assert_eq!(path, "/oauth2/token?v=2");
        // Its own target, so the token request gets its own pool rather than one of
        // the connections the load was given.
        assert_eq!(target.address, "idp.example.com:443");
        assert!(target.tls.enabled);

        let (_, plain) = token_target("at", "http://localhost:8080/token").unwrap();
        assert_eq!(plain.address, "localhost:8080");
        assert!(!plain.tls.enabled);

        assert!(token_target("at", "ftp://idp/token").is_err());
        assert!(token_target("at", "not a url").is_err());
    }

    #[test]
    fn a_credential_that_never_expires_is_not_refreshed_on_a_timer() {
        let held = Held {
            value: "x".into(),
            good_until: Some(Instant::now() - Duration::from_secs(1)),
        };
        // Past its deadline either way; the strategy is what decides whether that
        // means anything. A plan that says `never` is not second-guessed.
        assert!(expiring(&held, RefreshStrategy::ExpiresInMargin));
        assert!(!expiring(&held, RefreshStrategy::Never));
    }
}
