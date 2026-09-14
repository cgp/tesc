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
use crate::calls::RequestTemplate;
use crate::extract::{self, Extractor};
use crate::http::{Endpoint, Lease, Observation, SendState, Timing, send};
use crate::template::{Scope, Unbound};
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
    pub scheduled: Instant,
    pub admitted: Instant,
    pub send_state: Arc<SendState>,
}

/// What one step did.
pub(crate) struct Outcome {
    /// Index into the chain's steps, so the caller need not match on names.
    pub index: usize,
    pub observation: Observation,
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
}

impl Verdict {
    /// The assertion index, for the count the report keys by index.
    pub fn assertion(self) -> Option<usize> {
        match self {
            Self::Assertion(index) => Some(index),
            Self::Unfinished => None,
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
}

impl Completion {
    pub fn aborted(&self) -> bool {
        self.stopped.is_some()
    }

    /// The step that could not be built, and the variable it wanted.
    ///
    /// Reported against that step rather than as a run-wide note: the step was
    /// attempted and did not happen, which is exactly what a step's own failure
    /// count is for, and `extraction` is the class the frozen schema keeps for it.
    pub fn unbound(&self) -> Option<(&'static str, &str)> {
        match &self.stopped {
            Some(Stopped::Unbound { index, variable }) => {
                Some((self.chain.steps[*index].id, variable.as_str()))
            }
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
    let mut stopped = None;
    let mut truncated = false;

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

        let rendered = match render(step, &scope) {
            Ok(rendered) => rendered,
            Err(Unbound(variable)) => {
                stopped = Some(Stopped::Unbound { index, variable });
                break;
            }
        };

        let mut retried = false;
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
                steps.push(Outcome {
                    index,
                    observation,
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

            break Outcome {
                index,
                observation,
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
    }
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
fn render(step: &Step, scope: &Scope) -> Result<Option<crate::calls::Prepared>, Unbound> {
    if step.request.prepared().is_some() {
        return Ok(None);
    }
    step.request.render(scope).map(Some)
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
