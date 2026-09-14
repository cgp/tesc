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
//!
//! **A call that varies compiles to the pieces that vary; one that does not compiles
//! to the finished request.** A run with no chaining renders nothing per request, and
//! a chain pays only for the parts that actually hold a variable.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bytes::Bytes;
use hyper::{
    HeaderMap, Method, Uri,
    header::{HeaderName, HeaderValue},
    http::uri::Authority,
};
use metrix_plan::{Body, Call, Defaults, Mix, SessionPolicy, Target};

use crate::extract::Extractor;
use crate::template::{Scope, Template, Unbound};

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

/// A request, ready to send.
pub(crate) struct Prepared {
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// One call, compiled: the fixed parts once, the varying parts as templates.
pub(crate) struct RequestTemplate {
    pub method: Method,
    pub timeout: Duration,
    /// How much of a response may be kept, from the mix's `capture.body_max_kb`. A
    /// ceiling rather than a hope: a service that streams a gigabyte back must not
    /// be able to take the generator down through the extractor.
    body_max: usize,
    /// What this call captures out of its response, in the order it was written.
    pub extract: Vec<(String, Extractor)>,
    /// The whole request, when nothing in it depends on the scope. The ordinary
    /// case, and the one that must cost nothing per send.
    fixed: Option<Prepared>,

    origin: String,
    path: Template,
    query: Vec<(Template, Template)>,
    headers: Vec<(HeaderName, Template)>,
    host: HeaderValue,
    body: Template,
}

impl RequestTemplate {
    /// The finished request, for a call with no variables in it.
    pub fn prepared(&self) -> Option<&Prepared> {
        self.fixed.as_ref()
    }

    /// True when something has to read the response body.
    ///
    /// Asked per call rather than assumed: a load generator that buffers every
    /// response it receives is measuring its own allocator as much as the service.
    pub fn reads_body(&self) -> bool {
        self.extract
            .iter()
            .any(|(_, extractor)| !matches!(extractor, Extractor::Header(_)))
    }

    /// How many bytes of a response body may be kept.
    pub fn body_ceiling(&self) -> usize {
        self.body_max
    }

    /// True when the response's headers are read.
    pub fn reads_headers(&self) -> bool {
        self.extract
            .iter()
            .any(|(_, extractor)| matches!(extractor, Extractor::Header(_)))
    }

    /// Build this request from one iteration's scope.
    pub fn render(&self, scope: &Scope) -> Result<Prepared, Unbound> {
        let mut path = self.path.render(scope)?;
        for (key, value) in &self.query {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str(&key.render_query(scope)?);
            path.push('=');
            path.push_str(&value.render_query(scope)?);
        }
        let uri = format!("{}{path}", self.origin)
            .parse::<Uri>()
            // A captured value can hold anything the service chose to send. A path
            // that will not parse is the chain's to report, not a panic.
            .map_err(|_| Unbound(format!("the path rendered to {path:?}, which is not a URI")))?;

        let mut headers = HeaderMap::with_capacity(self.headers.len() + 1);
        for (name, value) in &self.headers {
            let rendered = value.render(scope)?;
            let value = HeaderValue::from_str(&rendered).map_err(|_| {
                Unbound(format!(
                    "header {name} rendered to something it cannot hold"
                ))
            })?;
            headers.insert(name.clone(), value);
        }
        headers.insert(hyper::header::HOST, self.host.clone());

        Ok(Prepared {
            uri,
            headers,
            body: Bytes::from(self.body.render(scope)?),
        })
    }

    /// One fixed request, for tests that need a template without a bundle.
    #[cfg(test)]
    pub fn fixed_for_test(method: Method, uri: Uri, body: Bytes, timeout: Duration) -> Self {
        let authority = uri.authority().expect("an absolute URI").clone();
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::HOST,
            HeaderValue::from_str(authority.as_str()).expect("a valid authority"),
        );
        Self {
            method,
            timeout,
            body_max: 64 * 1024,
            extract: Vec::new(),
            fixed: Some(Prepared { uri, headers, body }),
            origin: String::new(),
            path: Template::parse("test", "/").expect("a literal path"),
            query: Vec::new(),
            headers: Vec::new(),
            host: HeaderValue::from_static("test"),
            body: Template::parse("test", "").expect("an empty body"),
        }
    }

    /// Every variable this request reads, across all of its parts.
    fn variables(&self) -> impl Iterator<Item = &str> {
        self.path
            .variables()
            .chain(
                self.query
                    .iter()
                    .flat_map(|(key, value)| key.variables().chain(value.variables())),
            )
            .chain(self.headers.iter().flat_map(|(_, value)| value.variables()))
            .chain(self.body.variables())
    }
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
            let compiled = compile(
                &step.call,
                call,
                &mix.defaults,
                mix.capture.body_max_kb as usize * 1024,
                target,
                authority,
            )?;
            requests.insert(step.call.clone(), Arc::new(compiled));
        }
    }

    check_bindings(&chains, &requests)?;
    Ok(Resolved { chains, requests })
}

/// Every variable a step reads must have been written by a step before it.
///
/// Checked once, at load, because the alternative is finding out per iteration: a
/// chain whose second step reads `{{ order_id }}` that nothing captures fails every
/// time it runs, and reporting that as thousands of failed requests buries the one
/// fact that explains all of them.
fn check_bindings(
    chains: &[Chain],
    requests: &BTreeMap<String, Arc<RequestTemplate>>,
) -> Result<(), String> {
    for (index, chain) in chains.iter().enumerate() {
        let mut available: Vec<&str> = Vec::new();
        for (position, step) in chain.steps.iter().enumerate() {
            let request = &requests[&step.call];
            for name in request.variables() {
                require(
                    available.contains(&name),
                    &format!(
                        "mix.json/chains/{index}/steps/{position}: {{{{ {name} }}}} is read \
                         here and no earlier step of chain {:?} extracts it",
                        chain.name
                    ),
                )?;
            }
            available.extend(request.extract.iter().map(|(name, _)| name.as_str()));
        }
    }
    Ok(())
}

/// One call into the request it will send.
fn compile(
    name: &str,
    call: &Call,
    defaults: &Defaults,
    body_max: usize,
    target: &Target,
    authority: &Authority,
) -> Result<RequestTemplate, String> {
    let at = format!("call {name:?}");
    require(
        call.assertions.is_empty(),
        &format!("{at}: assertions are not implemented (B3.4)"),
    )?;

    let body = match &call.body {
        None => Template::parse(&format!("{at}/body"), "")?,
        Some(Body::Inline(text)) => Template::parse(&format!("{at}/body"), text)?,
        Some(Body::Generated { .. }) => {
            return Err(format!("{at}/body: generators are not implemented (B3.6)"));
        }
    };

    let path = Template::parse(&format!("{at}/path"), &call.path)?;
    require(
        call.path.starts_with('/') && !call.path.starts_with("//") && !call.path.contains('#'),
        &format!("{at}/path: expected an origin-relative path without a fragment"),
    )?;

    let mut query = Vec::new();
    for (key, value) in &call.query {
        query.push((
            Template::parse(&format!("{at}/query"), key)?,
            Template::parse(&format!("{at}/query"), value)?,
        ));
    }

    let mut headers = Vec::new();
    for (key, value) in defaults.headers.iter().chain(call.headers.iter()) {
        let key = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| format!("{at}/headers: invalid header name"))?;
        require(
            !TRANSPORT_MANAGED.contains(&key.as_str()),
            &format!("{at}/headers: {key} is managed by the transport and cannot be set"),
        )?;
        headers.push((key, Template::parse(&format!("{at}/headers"), value)?));
    }

    let mut extract = Vec::new();
    for (variable, selector) in &call.extract {
        let at = format!("{at}/extract/{variable}");
        require(
            !variable.is_empty(),
            &format!("{at}: an empty variable name"),
        )?;
        extract.push((variable.clone(), Extractor::compile(&at, selector)?));
    }

    let timeout = Duration::from_millis(call.timeout_ms.or(defaults.timeout_ms).unwrap_or(5000));
    require(
        !timeout.is_zero() && std::time::Instant::now().checked_add(timeout).is_some(),
        &format!("{at}/timeout_ms: must be positive and representable by the monotonic clock"),
    )?;

    let scheme = if target.tls.enabled { "https" } else { "http" };
    let host = HeaderValue::from_str(authority.as_str())
        .map_err(|_| "target: invalid HTTP authority".to_owned())?;

    let mut compiled = RequestTemplate {
        method: call.method.to_string().parse().expect("plan method enum"),
        timeout,
        body_max,
        extract,
        fixed: None,
        origin: format!("{scheme}://{authority}"),
        path,
        query,
        headers,
        host,
        body,
    };

    // Nothing varying means the request can be built now and sent unchanged for the
    // life of the run. Built through the same renderer the varying case uses, so
    // there is one way a request is assembled rather than two that can disagree.
    if compiled.variables().next().is_none() {
        let prepared = compiled
            .render(&Scope::new())
            .map_err(|Unbound(what)| format!("{at}: {what}"))?;
        compiled.fixed = Some(prepared);
    }

    Ok(compiled)
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
