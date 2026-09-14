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
use metrix_plan::{Call, Defaults, Mix, OnFailure, RepeatUntil, SessionPolicy, Target};

use crate::assertions::Check;
use crate::dataset::Datasets;
use crate::extract::Extractor;
use crate::generate::Generators;
use crate::template::{Scope, Template, Unbound, Values};

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

/// What a step reads out of a response on top of its call's own extractors.
///
/// Part of a compiled request's identity: two steps naming the same call but reading
/// its answer differently are the same request and different captures, and the cache
/// that shares compiled calls between steps has to tell them apart.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Reads {
    body: bool,
    headers: bool,
}

impl Reads {
    /// What a chain's session needs kept.
    ///
    /// Response headers, and only when something could read the cookies in them: a
    /// single-step `fresh` chain has no later request to send one to, so its jar
    /// could never be read and keeping the headers would be paying for nothing. Any
    /// other shape can carry state forward, and the engine cannot know whether a
    /// service sets a cookie without looking.
    pub fn session(policy: SessionPolicy, steps: usize) -> Self {
        Self {
            body: false,
            headers: steps > 1 || policy != SessionPolicy::Fresh,
        }
    }

    /// Everything, for a request whose whole answer is read: an auth response is
    /// small, arrives rarely, and every part of it may be what the plan asked for.
    pub fn all() -> Self {
        Self {
            body: true,
            headers: true,
        }
    }

    /// What a polling selector needs kept.
    fn of(extractor: Option<&Extractor>) -> Self {
        match extractor {
            None => Self::default(),
            Some(Extractor::Header(_)) => Self {
                body: false,
                headers: true,
            },
            Some(_) => Self {
                body: true,
                headers: false,
            },
        }
    }
}

/// True when a header belongs to the transport rather than to the plan.
pub(crate) fn transport_managed(name: &str) -> bool {
    TRANSPORT_MANAGED.contains(&name.to_ascii_lowercase().as_str())
}

impl std::ops::BitOr for Reads {
    type Output = Self;

    /// Two requirements over one request: whatever either of them needs kept.
    fn bitor(self, other: Self) -> Self {
        Self {
            body: self.body || other.body,
            headers: self.headers || other.headers,
        }
    }
}

/// A request, ready to send.
///
/// `Clone` for one reason: a request that carries a credential has to be copied
/// before it is stamped, because the compiled call is shared by every iteration and
/// the token is not.
#[derive(Clone)]
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
    /// What the response has to look like, in the order the call wrote them.
    pub assertions: Vec<Check>,
    /// What a step reads out of the answer beyond what the call itself declares —
    /// today, a `repeat_until` selector. Part of the request because capture is
    /// decided here: a polling step whose body was never kept would read nothing,
    /// find nothing, and poll to its ceiling against a service that answered
    /// correctly the first time.
    reads: Reads,
    /// The generator this call is built with, and the arguments it is handed. An
    /// index into the plan's generators, resolved at load like every other reference.
    pub generate: Option<Attachment>,
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
        self.reads.body
            || self
                .extract
                .iter()
                .any(|(_, extractor)| !matches!(extractor, Extractor::Header(_)))
            || self.assertions.iter().any(Check::reads_body)
    }

    /// How many bytes of a response body may be kept.
    pub fn body_ceiling(&self) -> usize {
        self.body_max
    }

    /// True when the response's headers are read.
    pub fn reads_headers(&self) -> bool {
        self.reads.headers
            || self
                .extract
                .iter()
                .any(|(_, extractor)| matches!(extractor, Extractor::Header(_)))
            || self.assertions.iter().any(Check::reads_headers)
    }

    /// Build this request from one iteration's scope.
    pub fn render(&self, values: &mut Values<'_>) -> Result<Prepared, Unbound> {
        let mut path = self.path.render(values)?;
        for (key, value) in &self.query {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str(&key.render_query(values)?);
            path.push('=');
            path.push_str(&value.render_query(values)?);
        }
        let uri = format!("{}{path}", self.origin)
            .parse::<Uri>()
            // A captured value can hold anything the service chose to send. A path
            // that will not parse is the chain's to report, not a panic.
            .map_err(|_| Unbound(format!("the path rendered to {path:?}, which is not a URI")))?;

        let mut headers = HeaderMap::with_capacity(self.headers.len() + 1);
        for (name, value) in &self.headers {
            let rendered = value.render(values)?;
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
            body: Bytes::from(self.body.render(values)?),
        })
    }

    /// Lay what a generator returned over what the call rendered.
    ///
    /// Only the parts it returned: a generator that had to return the whole request
    /// would force a plan to move its headers out of the call document and into a
    /// script to change one of them. A returned `query` replaces the call's rather
    /// than merging with it, because a generator building a query has decided what
    /// the request asks for and a leftover parameter underneath would be a request
    /// nobody wrote.
    pub fn apply(
        &self,
        prepared: &mut Prepared,
        built: crate::generate::Built,
    ) -> Result<(), String> {
        if built.path.is_some() || built.query.is_some() {
            let mut target = built.path.unwrap_or_else(|| prepared.uri.path().to_owned());
            match &built.query {
                Some(query) => {
                    for (key, value) in query {
                        target.push(if target.contains('?') { '&' } else { '?' });
                        target.push_str(&crate::template::encode(key));
                        target.push('=');
                        target.push_str(&crate::template::encode(value));
                    }
                }
                None => {
                    if let Some(existing) = prepared.uri.query() {
                        target.push('?');
                        target.push_str(existing);
                    }
                }
            }
            prepared.uri = format!("{}{target}", self.origin)
                .parse::<Uri>()
                .map_err(|_| format!("the generator built {target:?}, which is not a URI"))?;
        }
        for (name, value) in built.headers.into_iter().flatten() {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("the generator returned {name:?}, which is not a header"))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|_| format!("the generator returned a value {name} cannot hold"))?;
            prepared.headers.insert(name, value);
        }
        if let Some(body) = built.body {
            prepared.body = Bytes::from(body);
        }
        Ok(())
    }

    /// A request that is already finished, for the paths that do not come from a
    /// call document: an auth round trip, and the tests that need a template without
    /// a bundle behind it.
    pub fn fixed(
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
        timeout: Duration,
    ) -> Self {
        Self {
            method,
            timeout,
            body_max: 64 * 1024,
            extract: Vec::new(),
            assertions: Vec::new(),
            reads: Reads::all(),
            generate: None,
            fixed: Some(Prepared { uri, headers, body }),
            origin: String::new(),
            path: Template::parse("auth", "/", &Datasets::default()).expect("a literal path"),
            query: Vec::new(),
            headers: Vec::new(),
            host: HeaderValue::from_static("auth"),
            body: Template::parse("auth", "", &Datasets::default()).expect("an empty body"),
        }
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
            assertions: Vec::new(),
            reads: Reads::default(),
            generate: None,
            fixed: Some(Prepared { uri, headers, body }),
            origin: String::new(),
            path: Template::parse("test", "/", &Datasets::default()).expect("a literal path"),
            query: Vec::new(),
            headers: Vec::new(),
            host: HeaderValue::from_static("test"),
            body: Template::parse("test", "", &Datasets::default()).expect("an empty body"),
        }
    }

    /// Every variable this request reads, across all of its parts.
    fn variables(&self) -> impl Iterator<Item = &str> {
        self.pieces().flat_map(Template::variables)
    }

    /// Every dataset any part of this request reads.
    pub fn datasets(&self) -> impl Iterator<Item = usize> {
        self.pieces().flat_map(Template::datasets)
    }

    /// True when nothing in the request varies, so it can be built once.
    ///
    /// Asked of every piece rather than only of the chain variables: a path holding
    /// `{{ uuid() }}` reads no variable and is a different request every time. A call
    /// with a generator is never fixed — building it once would be calling the hook
    /// once, which is the opposite of what a generator is for.
    fn is_fixed(&self) -> bool {
        self.generate.is_none() && self.pieces().all(Template::is_fixed)
    }

    fn pieces(&self) -> impl Iterator<Item = &Template> {
        std::iter::once(&self.path)
            .chain(
                self.query
                    .iter()
                    .flat_map(|(key, value)| [key, value].into_iter()),
            )
            .chain(self.headers.iter().map(|(_, value)| value))
            .chain(std::iter::once(&self.body))
    }
}

/// A call's attachment to a generator.
pub(crate) struct Attachment {
    pub index: usize,
    /// Leaked, like the chain and step names, because it keys the accumulator map
    /// that holds this generator's own cost for the life of the run.
    pub name: &'static str,
    /// Templated like any other field, so a generator can be handed
    /// `{{ users.email }}` without knowing datasets exist.
    pub args: Vec<(String, Template)>,
}

/// One step of one chain, as the mix names it.
pub(crate) struct Step {
    pub id: String,
    pub call: String,
    /// The request this step sends. Shared with every other step that names the same
    /// call and reads its answer the same way, so a chain of ten steps over three
    /// calls compiles three requests.
    pub request: Arc<RequestTemplate>,
    pub on_failure: OnFailure,
    /// Compiled here rather than at the chain, because a selector that will not
    /// parse is a load-time error like any other.
    pub repeat_until: Option<crate::chain::Repeat>,
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
///
/// Only the calls some step names are compiled: a call nobody invokes is read and
/// parsed, and refusing the run over a path it declares would be refusing over a
/// request that is never sent.
pub(crate) struct Resolved {
    pub chains: Vec<Chain>,
}

impl Resolved {
    /// The longest any single request may take. The drain window has to be able to
    /// outlast it, so the timeline needs to know before it is built.
    pub fn longest_timeout(&self) -> Duration {
        self.chains
            .iter()
            .flat_map(|chain| &chain.steps)
            .map(|step| step.request.timeout)
            .max()
            .unwrap_or_default()
    }
}

/// Read the mixture's chains and compile every call they name.
pub(crate) fn resolve(
    mix: &Mix,
    defined: &BTreeMap<String, Call>,
    datasets: &Datasets,
    generators: &Generators,
    target: &Target,
    authority: &Authority,
) -> Result<Resolved, String> {
    let context = Context {
        defaults: &mix.defaults,
        body_max: mix.capture.body_max_kb as usize * 1024,
        datasets,
        generators,
        target,
        authority,
    };
    let mut chains = Vec::new();
    let mut seen_chains: BTreeMap<&str, usize> = BTreeMap::new();
    // Compiled once per call-and-capture, shared by every step that wants it.
    let mut compiled: BTreeMap<(String, Reads), Arc<RequestTemplate>> = BTreeMap::new();

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
            let repeat_until = step
                .repeat_until
                .as_ref()
                .map(|repeat| compile_repeat(&at, repeat))
                .transpose()?;
            let reads = Reads::of(repeat_until.as_ref().map(|repeat| &repeat.extractor))
                | Reads::session(chain.session, chain.steps.len());
            let key = (step.call.clone(), reads);
            let request = match compiled.get(&key) {
                Some(request) => Arc::clone(request),
                None => {
                    let request =
                        Arc::new(compile(&step.call, &defined[&step.call], reads, &context)?);
                    compiled.insert(key, Arc::clone(&request));
                    request
                }
            };
            steps.push(Step {
                id: step.id.clone(),
                call: step.call.clone(),
                request,
                on_failure: step.on_failure.unwrap_or_default(),
                repeat_until,
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

    check_bindings(&chains)?;
    Ok(Resolved { chains })
}

/// One `repeat_until` block, with its selector compiled and its ceiling checked.
fn compile_repeat(at: &str, repeat: &RepeatUntil) -> Result<crate::chain::Repeat, String> {
    require(
        repeat.max_attempts > 0,
        &format!("{at}/repeat_until/max_attempts: must be at least one"),
    )?;
    Ok(crate::chain::Repeat {
        extractor: Extractor::compile(&format!("{at}/repeat_until"), &repeat.selector)?,
        equals: match &repeat.equals {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        },
        max_attempts: repeat.max_attempts,
        interval: Duration::from_millis(repeat.interval_ms),
    })
}

/// Every variable a step reads must have been written by a step before it.
///
/// Checked once, at load, because the alternative is finding out per iteration: a
/// chain whose second step reads `{{ order_id }}` that nothing captures fails every
/// time it runs, and reporting that as thousands of failed requests buries the one
/// fact that explains all of them.
fn check_bindings(chains: &[Chain]) -> Result<(), String> {
    for (index, chain) in chains.iter().enumerate() {
        let mut available: Vec<&str> = Vec::new();
        for (position, step) in chain.steps.iter().enumerate() {
            let request = &step.request;
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

/// Everything outside a call that compiling one needs: the mix's defaults, where the
/// request is going, and what the plan makes available to a template.
pub(crate) struct Context<'a> {
    pub defaults: &'a Defaults,
    pub body_max: usize,
    pub datasets: &'a Datasets,
    pub generators: &'a Generators,
    pub target: &'a Target,
    pub authority: &'a Authority,
}

/// One call into the request it will send.
pub(crate) fn compile(
    name: &str,
    call: &Call,
    reads: Reads,
    context: &Context<'_>,
) -> Result<RequestTemplate, String> {
    let Context {
        defaults,
        body_max,
        datasets,
        generators,
        target,
        authority,
    } = *context;
    let at = format!("call {name:?}");

    let body = Template::parse(
        &format!("{at}/body"),
        call.body.as_deref().unwrap_or_default(),
        datasets,
    )?;

    let generate = call
        .generate
        .as_ref()
        .map(|generate| {
            let at = format!("{at}/generate");
            let mut args = Vec::new();
            for (name, value) in &generate.args {
                let at = format!("{at}/args/{name}");
                // Strings are templated; anything else is passed through as the text
                // it serialises to, because a hook receives strings either way.
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                args.push((name.clone(), Template::parse(&at, &text, datasets)?));
            }
            let index = generators.resolve(&at, &generate.generator)?;
            Ok::<_, String>(Attachment {
                index,
                name: generators.leaked(index),
                args,
            })
        })
        .transpose()?;

    let path = Template::parse(&format!("{at}/path"), &call.path, datasets)?;
    require(
        call.path.starts_with('/') && !call.path.starts_with("//") && !call.path.contains('#'),
        &format!("{at}/path: expected an origin-relative path without a fragment"),
    )?;

    let mut query = Vec::new();
    for (key, value) in &call.query {
        query.push((
            Template::parse(&format!("{at}/query"), key, datasets)?,
            Template::parse(&format!("{at}/query"), value, datasets)?,
        ));
    }

    let mut headers = Vec::new();
    for (key, value) in defaults.headers.iter().chain(call.headers.iter()) {
        let key = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| format!("{at}/headers: invalid header name"))?;
        require(
            !transport_managed(key.as_str()),
            &format!("{at}/headers: {key} is managed by the transport and cannot be set"),
        )?;
        headers.push((
            key,
            Template::parse(&format!("{at}/headers"), value, datasets)?,
        ));
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

    let mut assertions = Vec::new();
    for (index, assertion) in call.assertions.iter().enumerate() {
        assertions.push(Check::compile(&format!("{at}/assert/{index}"), assertion)?);
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
        assertions,
        reads,
        generate,
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
    if compiled.is_fixed() {
        let scope = Scope::new();
        let mut values = Values::new(&scope, datasets, 0, 0);
        let prepared = compiled
            .render(&mut values)
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
