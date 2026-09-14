//! Resolving `call` references from the mix, and compiling what they name.
//!
//! The two documents are separate because they are authored by different parties and
//! change at different rates (design-engine §4): the calls describe the service and
//! go stale when it changes, the mix describes the question and does not. Nothing in
//! a call knows how often it runs or what runs before it, and nothing in the mix
//! knows what a request looks like. This module is the one place the two meet.
//!
//! **Every step of every chain is resolved, not just the one that will run.** A
//! bundle whose fourth chain names a call that does not exist is broken now, and
//! finding that out at load time rather than four minutes into a run is the whole
//! point of compiling ahead.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bytes::Bytes;
use hyper::{
    HeaderMap, Method, Uri,
    header::{HeaderName, HeaderValue},
    http::uri::Authority,
};
use metrix_plan::{Body, Call, Defaults, Mix, SessionPolicy, Target};

/// Headers the transport owns. A plan that sets one of these is describing a
/// different request from the one that would go on the wire.
const TRANSPORT_MANAGED: &[&str] = &[
    "host",
    "connection",
    "proxy-connection",
    "keep-alive",
    "upgrade",
    "transfer-encoding",
    "content-length",
    "te",
    "trailer",
];

/// One request, compiled once and sent many times.
pub(crate) struct RequestTemplate {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub timeout: Duration,
}

/// One step of one chain, as the mix names it.
pub(crate) struct Step {
    pub id: String,
    pub call: String,
}

/// One chain, with its steps resolved to calls that exist.
pub(crate) struct Chain {
    pub name: String,
    pub percent: f64,
    /// Whether the chain needs a fresh session. A property of the behaviour being
    /// modelled rather than of the service (§4.2), so it travels with the chain.
    pub session: SessionPolicy,
    pub pool_size: Option<u32>,
    pub steps: Vec<Step>,
}

/// What the mix asked for, resolved against what the calls document defines.
pub(crate) struct Resolved {
    pub chains: Vec<Chain>,
    /// Call name to the request it compiles to. Only the calls some step names: a
    /// call nobody invokes is read and parsed, and refusing the run over a path it
    /// declares would be refusing over a request that is never sent.
    pub requests: BTreeMap<String, Arc<RequestTemplate>>,
}

impl Resolved {
    /// The longest any single request may take. The drain window has to be able to
    /// outlast it, so the timeline needs to know before it is built.
    pub fn longest_timeout(&self) -> Duration {
        self.requests
            .values()
            .map(|request| request.timeout)
            .max()
            .unwrap_or_default()
    }
}

/// Read the mixture's chains and compile every call they name.
pub(crate) fn resolve(
    mix: &Mix,
    defined: &BTreeMap<String, Call>,
    target: &Target,
    authority: &Authority,
) -> Result<Resolved, String> {
    let mut chains = Vec::new();
    let mut seen_chains: BTreeMap<&str, usize> = BTreeMap::new();

    for (index, chain) in mix.chains.iter().enumerate() {
        let at = format!("mix.json/chains/{index}");
        require(
            !chain.name.is_empty(),
            &format!("{at}/name: must not be empty"),
        )?;
        if let Some(first) = seen_chains.insert(&chain.name, index) {
            // The name keys every chart series, error report and SLO. Two chains
            // sharing one is a report that cannot say which of them it is about.
            return Err(format!(
                "{at}/name: duplicates chains/{first}/name; chain names identify their \
                 own measurements"
            ));
        }
        require(
            !chain.steps.is_empty(),
            &format!("{at}/steps: a chain with no steps sends nothing"),
        )?;

        let mut steps = Vec::new();
        let mut seen_steps: BTreeMap<&str, usize> = BTreeMap::new();
        for (position, step) in chain.steps.iter().enumerate() {
            let at = format!("{at}/steps/{position}");
            require(!step.id.is_empty(), &format!("{at}/id: must not be empty"))?;
            if let Some(first) = seen_steps.insert(&step.id, position) {
                return Err(format!(
                    "{at}/id: duplicates steps/{first}/id; a step id is how one step's \
                     own latency is reported"
                ));
            }
            require(
                defined.contains_key(&step.call),
                &format!("{at}/call: no call named {:?} is defined", step.call),
            )?;
            steps.push(Step {
                id: step.id.clone(),
                call: step.call.clone(),
            });
        }
        chains.push(Chain {
            name: chain.name.clone(),
            percent: chain.percent,
            session: chain.session,
            pool_size: chain.pool_size,
            steps,
        });
    }

    let mut requests = BTreeMap::new();
    for chain in &chains {
        for step in &chain.steps {
            if requests.contains_key(&step.call) {
                continue;
            }
            let call = &defined[&step.call];
            let compiled = compile(&step.call, call, &mix.defaults, target, authority)?;
            requests.insert(step.call.clone(), Arc::new(compiled));
        }
    }

    Ok(Resolved { chains, requests })
}

/// One call into the request it will send, every time, unchanged.
fn compile(
    name: &str,
    call: &Call,
    defaults: &Defaults,
    target: &Target,
    authority: &Authority,
) -> Result<RequestTemplate, String> {
    let at = format!("call {name:?}");
    require(
        call.assertions.is_empty() && call.extract.is_empty(),
        &format!("{at}: assertions and extraction are not implemented (B3.4, B3.2)"),
    )?;

    let body = match &call.body {
        None => Bytes::new(),
        Some(Body::Inline(text)) => {
            static_text(&at, "body", text)?;
            Bytes::copy_from_slice(text.as_bytes())
        }
        Some(Body::Generated { .. }) => {
            return Err(format!("{at}/body: generators are not implemented (B3.6)"));
        }
    };

    static_text(&at, "path", &call.path)?;
    require(
        call.path.starts_with('/') && !call.path.starts_with("//") && !call.path.contains('#'),
        &format!("{at}/path: expected an origin-relative path without a fragment"),
    )?;

    let mut path = call.path.clone();
    for (key, value) in &call.query {
        static_text(&at, "query", key)?;
        static_text(&at, "query", value)?;
        path.push(if path.contains('?') { '&' } else { '?' });
        path.push_str(&encode_query(key));
        path.push('=');
        path.push_str(&encode_query(value));
    }

    let scheme = if target.tls.enabled { "https" } else { "http" };
    let uri = format!("{scheme}://{authority}{path}")
        .parse::<Uri>()
        .map_err(|_| format!("{at}/path: invalid HTTP URI"))?;

    let mut headers = HeaderMap::new();
    for (key, value) in defaults.headers.iter().chain(call.headers.iter()) {
        static_text(&at, "headers", value)?;
        let key = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| format!("{at}/headers: invalid header name"))?;
        require(
            !TRANSPORT_MANAGED.contains(&key.as_str()),
            &format!("{at}/headers: {key} is managed by the transport and cannot be set"),
        )?;
        headers.insert(
            key,
            HeaderValue::from_str(value)
                .map_err(|_| format!("{at}/headers: invalid header value"))?,
        );
    }
    headers.insert(
        hyper::header::HOST,
        HeaderValue::from_str(authority.as_str()).map_err(|_| "target: invalid HTTP authority")?,
    );

    let timeout = Duration::from_millis(call.timeout_ms.or(defaults.timeout_ms).unwrap_or(5000));
    require(
        !timeout.is_zero() && std::time::Instant::now().checked_add(timeout).is_some(),
        &format!("{at}/timeout_ms: must be positive and representable by the monotonic clock"),
    )?;

    Ok(RequestTemplate {
        method: call.method.to_string().parse().expect("plan method enum"),
        uri,
        headers,
        body,
        timeout,
    })
}

fn static_text(at: &str, field: &str, value: &str) -> Result<(), String> {
    require(
        !value.contains("{{") && !value.contains("}}"),
        &format!("{at}/{field}: templates are not implemented (B3.5)"),
    )
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn encode_query(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}
