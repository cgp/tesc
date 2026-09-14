//! One iteration of one chain: the steps in order, down one connection, with one
//! variable scope.
//!
//! A chain is a sequential chain executed by one virtual user (design-engine §5).
//! That sentence decides three things here:
//!
//! - **One connection for the whole iteration.** Giving each step its own socket
//!   would measure a service being connected to rather than a service being used,
//!   and it is not what the behaviour being modelled does.
//! - **One scope, thrown away at the end.** Two iterations are two different people
//!   as far as the service is concerned, so nothing a step captures outlives the
//!   iteration that captured it.
//! - **A step that fails stops the chain.** The steps after it were going to act on
//!   something that did not happen; sending them would measure a flow the service
//!   never got into, and count their failures as separate problems.

use std::{sync::Arc, time::Duration};

use tokio::time::Instant;

use crate::assertions;
use crate::auth;
use crate::calls::RequestTemplate;
use crate::dataset::Datasets;
use crate::extract::{self, Extractor};
use crate::generate::{self, Generators};
use crate::http::{Endpoint, Lease, Observation, SendState, Timing, send};
use crate::template::{Scope, Unbound, Values};
use metrix_metrics::aggregation::Cause;
use metrix_plan::OnFailure;

/// One step, ready to run.
///
/// The id and the chain's name are `&'static str` because they live for the whole
/// run and key every accumulator map. Leaked once when the plan compiles, so
/// recording a step costs a map lookup rather than a string clone.
pub(crate) struct Step {
    pub id: &'static str,
    /// Which call this step invokes. The id is the step's own name and the call is
    /// the request's; a report that carried only one of them could not say either
    /// which step was slow or which request it sent.
    pub call: &'static str,
    pub request: Arc<RequestTemplate>,
    /// What to do when this step does not succeed. Aborting is the default because
    /// the steps after it were going to act on something that did not happen.
    pub on_failure: OnFailure,
    /// The async-job pattern: keep asking until the answer says it is done.
    pub repeat_until: Option<Repeat>,
}

/// Poll a step until a value in its response says the work finished.
///
/// A loop without a loop construct: the plan says what to look for, how often, and
/// how many times, and nothing about it is open-ended. Every attempt is a real
/// request and is counted as one -- what must not contaminate request latency is the
/// waiting between them, which lands in the chain's end-to-end duration instead.
pub(crate) struct Repeat {
    pub extractor: Extractor,
    pub equals: String,
    pub max_attempts: u32,
    pub interval: Duration,
}

/// One chain, ready to run, shared by every slot that runs it.
pub(crate) struct Compiled {
    pub name: &'static str,
    pub steps: Vec<Step>,
}

pub(crate) struct Job {
    pub lease: Lease,
    pub endpoint: Arc<Endpoint>,
    pub chain: Arc<Compiled>,
    /// Every dataset the plan declares, read once at load and shared by every
    /// iteration for the life of the run.
    pub datasets: Arc<Datasets>,
    /// Every generator the plan declares, started before the arrival clock did.
    pub generators: Arc<Generators>,
    /// The credential every request carries, with its tokens already fetched.
    pub auth: Option<Arc<crate::auth::Auth>>,
    /// The error-sample budget, so a failing step knows whether to keep a copy of
    /// what it sent.
    pub samples: Arc<crate::samples::Samples>,
    /// This chain's sessions, and which of them this iteration is.
    pub sessions: crate::session::PerChain,
    pub chain_index: usize,
    /// The run seed, recorded in the run's identity. Together with the iteration
    /// number it decides every generated value this iteration sends.
    pub seed: u64,
    pub iteration: u64,
    /// Which virtual user this iteration is, for a generator that wants to model one
    /// person doing several things.
    pub vu: usize,
    pub scheduled: Instant,
    pub admitted: Instant,
    pub send_state: Arc<SendState>,
}

/// What one step did.
pub(crate) struct Outcome {
    /// Index into the chain's steps, so the caller need not match on names.
    pub index: usize,
    pub observation: Observation,
    /// The request exactly as it went out, kept only when something failed and the
    /// sample budget still had room for it (§9.3). `None` the rest of the time,
    /// because holding a copy of every request is holding every request.
    pub sent: Option<crate::calls::Prepared>,
    /// How the answer itself fell short, if it did. The request succeeded; what is
    /// wrong is the answer, and the two are counted apart.
    pub verdict: Option<Verdict>,
}

/// A response that arrived and was not the one the plan asked for.
#[derive(Clone, Copy)]
pub(crate) enum Verdict {
    /// The assertion at this index in the call did not hold.
    Assertion(usize),
    /// `repeat_until` ran out of attempts and the value never said finished. A step
    /// that polled five times and gave up has not seen the job complete, and calling
    /// that a success would report a service that finishes nothing as healthy.
    Unfinished,
    /// The target rejected the credential and the plan says that is a failure rather
    /// than a renewal. Its own class (§6.1): a 401 is not an application error, and a
    /// run that reported it as one would send somebody reading the service's code.
    Unauthorized,
}

impl Verdict {
    /// The assertion index, for the count the report keys by index.
    pub fn assertion(self) -> Option<usize> {
        match self {
            Self::Assertion(index) => Some(index),
            Self::Unfinished | Self::Unauthorized => None,
        }
    }

    /// The class this verdict is counted under.
    pub fn cause(self) -> Cause {
        match self {
            Self::Assertion(_) | Self::Unfinished => Cause::Assertion,
            Self::Unauthorized => Cause::Unauthorized,
        }
    }
}

/// Why an iteration ended before its last step.
pub(crate) enum Stopped {
    /// A step did not succeed and its `on_failure` was not `continue`. Abort is the
    /// default (§5), and a retry that failed twice lands here too.
    Failed,
    /// A step needed a value that no response before it provided. Distinct from a
    /// failed request because nothing was sent: the plan asked for something it had
    /// not captured, and reporting it as a transport error would point at the
    /// service.
    Unbound { index: usize, variable: String },
    /// A generator could not build the request (§7.3). Also nothing sent, and also
    /// ours rather than theirs -- but a different class, because a plan's own script
    /// failing is not the same problem as a plan referring to a value it never took.
    Generation { index: usize, reason: String },
    /// No usable credential, so the request was never made. Counted as unauthorized
    /// rather than as a transport failure: the service was not asked, and the reason
    /// is on this side of the connection.
    Credential { index: usize, reason: String },
}

/// How one generator call went, carried out so the caller can record it against the
/// run's own health rather than against the service.
pub(crate) struct Generated {
    pub generator: &'static str,
    pub took: Duration,
    pub failed: bool,
}

pub(crate) struct Completion {
    pub lease: Lease,
    pub chain: Arc<Compiled>,
    pub steps: Vec<Outcome>,
    /// End to end, admission to last response. Not the sum of the steps: the gaps
    /// between them are part of what a user waits through.
    pub duration: Duration,
    /// How far behind its scheduled arrival the iteration was admitted.
    pub admission_delay: Duration,
    /// True when a response was longer than the capture ceiling and was cut. Carried
    /// because it changes what a missing variable means: an extractor that found
    /// nothing in a truncated document may have been looking past the cut.
    pub truncated: bool,
    pub stopped: Option<Stopped>,
    /// Every generator call this iteration made, in order.
    pub generated: Vec<Generated>,
}

impl Completion {
    pub fn aborted(&self) -> bool {
        self.stopped.is_some()
    }

    /// The step that could not be built, why, and the class it is counted under.
    ///
    /// Reported against that step rather than as a run-wide note: the step was
    /// attempted and did not happen, which is exactly what a step's own failure count
    /// is for. Two classes, because a plan referring to a value it never captured and
    /// a plan's own script failing are different problems to go and fix.
    pub fn not_sent(&self) -> Option<(&'static str, Cause, &str)> {
        match &self.stopped {
            Some(Stopped::Unbound { index, variable }) => Some((
                self.chain.steps[*index].id,
                Cause::Extraction,
                variable.as_str(),
            )),
            Some(Stopped::Generation { index, reason }) => Some((
                self.chain.steps[*index].id,
                Cause::Generation,
                reason.as_str(),
            )),
            Some(Stopped::Credential { index, reason }) => Some((
                self.chain.steps[*index].id,
                Cause::Unauthorized,
                reason.as_str(),
            )),
            _ => None,
        }
    }

    /// True when a response in this iteration was cut at the capture ceiling.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

/// Which chain each arrival runs.
///
/// Deterministic and exactly proportional, rather than a weighted coin. A percentage
/// in a mixture is a claim about what the service was asked for, and a run that got
/// 19.3% instead of 20% because of sampling noise has measured a mixture nobody
/// wrote. The same plan therefore also produces the same interleaving twice, which
/// is what makes two runs of it comparable at all.
///
/// Smooth weighted round-robin: every arrival adds each chain's share to its credit,
/// the largest credit wins, and the winner pays the total back. Counts stay within
/// one of their exact share at every point in the run, not merely at the end.
pub(crate) struct Mixture {
    weights: Vec<f64>,
    credit: Vec<f64>,
    total: f64,
}

impl Mixture {
    pub fn new(weights: Vec<f64>) -> Self {
        let total = weights.iter().sum();
        Self {
            credit: vec![0.0; weights.len()],
            weights,
            total,
        }
    }

    pub fn next(&mut self) -> usize {
        for (credit, weight) in self.credit.iter_mut().zip(&self.weights) {
            *credit += weight;
        }
        let best = self
            .credit
            .iter()
            .enumerate()
            .max_by(|left, right| {
                left.1
                    .partial_cmp(right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or(0, |(index, _)| index);
        self.credit[best] -= self.total;
        best
    }
}

/// Run one iteration. `None` parks the slot: one future type per slot, reused.
pub(crate) async fn run(job: Option<Job>) -> Completion {
    let Some(mut job) = job else {
        return std::future::pending().await;
    };
    let mut scope = Scope::new();
    let mut steps = Vec::with_capacity(job.chain.steps.len());
    // Taken once for the iteration and held for it: a session is one virtual user
    // working through one conversation, and swapping identities between two steps of
    // one chain would be two people sharing a checkout.
    let sessions = &job.sessions[job.chain_index];
    let session = sessions.of(job.vu, job.iteration);
    let mut jar = sessions.open(session).await;
    let mut stopped = None;
    let mut truncated = false;
    let mut generated = Vec::new();

    for (index, step) in job.chain.steps.iter().enumerate() {
        let mut attempts = 0;
        // The first step is the one the schedule is measured against: it is the
        // arrival that was due. A later step is late because the service was slow,
        // which is the measurement rather than a fault in the generator.
        let first = index == 0;
        let now = Instant::now();
        let timing = Timing {
            scheduled: if first { job.scheduled } else { now },
            admitted: if first { job.admitted } else { now },
            records_drift: first,
        };

        let built = match build(step, &scope, &job, &mut generated).await {
            Ok(rendered) => rendered,
            Err(Refused::Unbound(variable)) => {
                stopped = Some(Stopped::Unbound { index, variable });
                break;
            }
            Err(Refused::Generation(reason)) => {
                stopped = Some(Stopped::Generation { index, reason });
                break;
            }
        };
        // A request that carries a credential is not a fixed request, however fixed
        // the call is: the token changes when it is refreshed, so the finished
        // request has to be copied and stamped rather than sent as it was compiled.
        let (mut rendered, mut carried) = match outgoing(step, built, &job, session, &jar).await {
            Ok(pair) => pair,
            Err(reason) => {
                stopped = Some(Stopped::Credential { index, reason });
                break;
            }
        };

        let mut retried = false;
        let mut renewed = false;
        let outcome = loop {
            attempts += 1;
            let observation = send(
                &mut job.lease,
                &job.endpoint,
                &step.request,
                rendered.as_ref(),
                timing,
                &job.send_state,
            )
            .await;
            truncated |= observation
                .response
                .as_ref()
                .is_some_and(|captured| captured.truncated);
            // What the service asked this session to remember. Read before the answer
            // is judged, because a step that fails an assertion was still told it.
            if let Some(captured) = &observation.response {
                jar.absorb(&captured.headers);
            }

            // Auth before the assertions: a 401 is the credential's business first,
            // and judging the body of a rejection would be judging the wrong thing.
            if observation.status == Some(401)
                && let Some(auth) = &job.auth
            {
                if auth.fails_on_401() {
                    let sent = keep(&job, true, &rendered, step);
                    break Outcome {
                        index,
                        observation,
                        sent,
                        verdict: Some(Verdict::Unauthorized),
                    };
                }
                if !renewed {
                    renewed = true;
                    let who = auth::Who {
                        session: session.id,
                        iteration: job.iteration,
                        seed: job.seed,
                        datasets: &job.datasets,
                    };
                    // Single-flight, inside `on_unauthorized`: two hundred users
                    // rejected at once send one refresh between them.
                    let rejected = carried.clone().unwrap_or_default();
                    if auth.on_unauthorized(who, &rejected).await == auth::Retry::Yes {
                        let mut again = rendered
                            .clone()
                            .expect("a request carrying a credential was rendered");
                        if let Ok(token) = auth.inject(&mut again, who).await {
                            // A 401 that is about to be renewed and retried: the
                            // request happened, and the sample is worth having even
                            // though the step recovers.
                            let sent = keep(&job, true, &rendered, step);
                            steps.push(Outcome {
                                index,
                                observation,
                                sent,
                                verdict: None,
                            });
                            rendered = Some(again);
                            carried = Some(token);
                            continue;
                        }
                    }
                }
            }

            // The transport first, then the answer: a step that never got a reply
            // has nothing for an assertion to be about.
            let mut verdict = if observation.error.is_some() {
                None
            } else {
                judge(step, &observation).map(Verdict::Assertion)
            };
            let failed = observation.error.is_some() || verdict.is_some();

            if failed && step.on_failure == OnFailure::Retry && !retried {
                // Once. The format says `retry` without a number, and a generator
                // that decided on its own how many times to hammer a failing service
                // would be choosing the load rather than running the plan.
                retried = true;
                let sent = keep(&job, true, &rendered, step);
                steps.push(Outcome {
                    index,
                    observation,
                    sent,
                    verdict,
                });
                continue;
            }

            if let (false, Some(repeat)) = (failed, &step.repeat_until) {
                match poll(repeat, &observation) {
                    Poll::Finished => {}
                    Poll::Again if attempts < repeat.max_attempts => {
                        steps.push(Outcome {
                            index,
                            observation,
                            sent: None,
                            verdict,
                        });
                        // Outside the request: what must not contaminate request
                        // latency is the waiting between attempts, which belongs to
                        // the chain's end-to-end duration instead.
                        tokio::time::sleep(repeat.interval).await;
                        continue;
                    }
                    Poll::Again => verdict = Some(Verdict::Unfinished),
                }
            }

            // The same rule the sample is filed under, so the two cannot disagree:
            // a 500 nobody asserted against is neither a transport error nor a failed
            // verdict, and it is exactly the answer somebody will want to read.
            let sent = keep(
                &job,
                crate::samples::classify(&observation, verdict).is_some(),
                &rendered,
                step,
            );
            break Outcome {
                index,
                observation,
                sent,
                verdict,
            };
        };

        let failed = outcome.observation.error.is_some() || outcome.verdict.is_some();
        if !failed {
            capture(step, &outcome.observation, &mut scope);
        }
        let policy = step.on_failure;
        steps.push(outcome);
        if failed && policy != OnFailure::Continue {
            stopped = Some(Stopped::Failed);
            break;
        }
    }

    Completion {
        duration: Instant::now().saturating_duration_since(job.admitted),
        admission_delay: job.admitted.saturating_duration_since(job.scheduled),
        lease: job.lease,
        chain: job.chain,
        steps,
        truncated,
        stopped,
        generated,
    }
}

/// Attach the credential, if the plan has one.
///
/// Returns the request to send: `None` still means "send the call as compiled", which
/// is the ordinary case for a plan with no auth and the one that must cost nothing.
/// A copy of the request, when something failed and a sample might want it.
///
/// Cheap by construction: `Prepared` is a URI, a header map and a `Bytes`, and this
/// is reached only on a failure while the budget still has room. Every other request
/// keeps nothing, which is the point — a generator holding a copy of each request it
/// sends is holding every request it sends.
fn keep(
    job: &Job,
    failed: bool,
    rendered: &Option<crate::calls::Prepared>,
    step: &Step,
) -> Option<crate::calls::Prepared> {
    if !failed || !job.samples.wants_bodies() {
        return None;
    }
    rendered
        .clone()
        .or_else(|| step.request.prepared().cloned())
}

/// The request as it actually goes out: what the session carries, on top of what the
/// call says.
///
/// `None` still means "send the call exactly as it compiled", which is the ordinary
/// case for a plan with no auth and no cookies and the one that must cost nothing.
/// Anything a session adds forces a copy, because the compiled call is shared by
/// every iteration and a credential is not.
async fn outgoing(
    step: &Step,
    built: Option<crate::calls::Prepared>,
    job: &Job,
    session: crate::session::Session,
    jar: &crate::session::Jar,
) -> Result<(Option<crate::calls::Prepared>, Option<String>), String> {
    let path = built
        .as_ref()
        .map(|prepared| prepared.uri.path())
        .or_else(|| step.request.prepared().map(|prepared| prepared.uri.path()))
        .unwrap_or("/");
    let cookies = jar.header(path);
    if job.auth.is_none() && cookies.is_none() {
        return Ok((built, None));
    }
    let mut prepared = match built {
        Some(prepared) => prepared,
        None => step
            .request
            .prepared()
            .expect("a call that rendered nothing is a fixed call")
            .clone(),
    };
    if let Some(header) = cookies
        && let Ok(value) = hyper::header::HeaderValue::from_str(&header)
    {
        prepared.headers.insert(hyper::header::COOKIE, value);
    }
    let Some(auth) = &job.auth else {
        return Ok((Some(prepared), None));
    };
    // Bound to the session rather than to the slot: a fresh session that reused a
    // token would not be fresh in any way the service can tell (§4.2).
    let token = auth
        .inject(
            &mut prepared,
            auth::Who {
                session: session.id,
                iteration: job.iteration,
                seed: job.seed,
                datasets: &job.datasets,
            },
        )
        .await?;
    Ok((Some(prepared), Some(token)))
}

/// Why a request could not be built at all, so none was sent.
enum Refused {
    Unbound(String),
    Generation(String),
}

/// Whether this answer said the work is done.
enum Poll {
    Finished,
    Again,
}

/// Read the state the plan is waiting on out of one answer.
///
/// A response the selector cannot find is not finished either: it is a job still
/// running that has not published its state yet. Whether there is another attempt
/// left is the caller's question, because running out is a different outcome from
/// being told to wait.
fn poll(repeat: &Repeat, observation: &Observation) -> Poll {
    let Some(captured) = &observation.response else {
        return Poll::Again;
    };
    let response = extract::Response::new(&captured.headers, &captured.body);
    if response.read(&repeat.extractor).as_deref() == Some(repeat.equals.as_str()) {
        Poll::Finished
    } else {
        Poll::Again
    }
}

/// What the plan says this answer had to look like.
fn judge(step: &Step, observation: &Observation) -> Option<usize> {
    if step.request.assertions.is_empty() {
        return None;
    }
    let captured = observation.response.as_ref();
    let response = captured.map(|c| extract::Response::new(&c.headers, &c.body));
    assertions::evaluate(
        &step.request.assertions,
        observation.status,
        observation.request_duration,
        response.as_ref(),
    )
}

/// Build the request, or nothing when the call never varies.
///
/// One `Values` per step rather than one per iteration: the generated values a step
/// sends are keyed to the run seed and the iteration, so every step of an iteration
/// draws the same stream from the start. Two steps posting `{{ uuid() }}` therefore
/// send the same id, which is what a chain creating a resource and then reading it
/// back needs — and a replay of the plan sends it again.
async fn build(
    step: &Step,
    scope: &Scope,
    job: &Job,
    generated: &mut Vec<Generated>,
) -> Result<Option<crate::calls::Prepared>, Refused> {
    let Some(declared) = step.request.generate.as_ref() else {
        if step.request.prepared().is_some() {
            return Ok(None);
        }
        let mut values = Values::new(scope, &job.datasets, job.seed, job.iteration);
        return step
            .request
            .render(&mut values)
            .map(Some)
            .map_err(|Unbound(what)| Refused::Unbound(what));
    };

    // Substitution first, because a generator's arguments are templated too: a call
    // handing its hook `{{ users.email }}` gets the row's email, and the hook never
    // has to learn that datasets exist.
    let mut values = Values::new(scope, &job.datasets, job.seed, job.iteration);
    let mut prepared = step
        .request
        .render(&mut values)
        .map_err(|Unbound(what)| Refused::Unbound(what))?;
    let mut args = std::collections::BTreeMap::new();
    for (name, template) in &declared.args {
        let rendered = template
            .render(&mut values)
            .map_err(|Unbound(what)| Refused::Unbound(what))?;
        args.insert(name.clone(), rendered);
    }

    let name = job.generators.name(declared.index);
    let rows: Vec<_> = job
        .datasets
        .iter()
        .map(|set| {
            (
                set.name(),
                set.fields(set.row(job.iteration, job.seed)).collect(),
            )
        })
        .collect();
    let mut context = generate::Context {
        vu: job.vu,
        iteration: job.iteration,
        step: step.id,
        vars: scope,
        rows,
        args: &args,
        rng: &mut values.rng,
    };

    // Timed around the hook alone and kept out of request latency: if generation is
    // the slow part it has to be visible as generation (§7.3), and folded into the
    // response time it would make the service look slow instead.
    let started = std::time::Instant::now();
    // Boxed: this future is inlined into the chain iteration's, which is built on the
    // scheduler's stack before it is moved into its slot. Without the box the state
    // machine carries the largest tier's whole exchange -- a child process, its pipes
    // and its buffers -- in every iteration of every chain, generated or not.
    let built = Box::pin(job.generators.get(declared.index).build(&mut context)).await;
    let took = started.elapsed();
    let outcome = built.and_then(|built| {
        generate::validate(name, &built)?;
        step.request.apply(&mut prepared, built)
    });
    generated.push(Generated {
        generator: declared.name,
        took,
        failed: outcome.is_err(),
    });
    outcome.map_err(Refused::Generation)?;
    Ok(Some(prepared))
}

/// Read this step's captures into the scope.
///
/// A selector that matched nothing leaves its variable unset rather than setting it
/// empty. The step that needed it then fails naming the variable, which is a
/// different and more useful report than a request to `/orders/` answered with a 404.
fn capture(step: &Step, observation: &Observation, scope: &mut Scope) {
    if step.request.extract.is_empty() {
        return;
    }
    let Some(captured) = &observation.response else {
        return;
    };
    let response = extract::Response::new(&captured.headers, &captured.body);
    for (name, extractor) in &step.request.extract {
        if let Some(value) = response.read(extractor) {
            scope.insert(name.clone(), value);
        }
    }
}
