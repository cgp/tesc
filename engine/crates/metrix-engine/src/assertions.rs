//! What a response has to look like for a step to have succeeded.
//!
//! Declarative and enumerable: every variant is a shape a model can emit from a
//! schema, and every failure names the assertion that failed by its index in the
//! call. There is no expression language, because an expression language turns a
//! failed assertion into a debugging session about the expression.
//!
//! **An assertion failure is not a transport failure.** The request happened and the
//! service answered; what is wrong is that the answer was not the one the plan
//! expects. They are counted apart for the same reason aborted chains are counted
//! apart from failed requests: one upstream problem should appear once.
//!
//! **A chain that expects a 401 passes when it gets one.** `login-fail` asserting
//! `{"status": 401}` is a deliberately-failing flow, and a generator that called its
//! own expectation an error would report a working service as broken.

use std::time::Duration;

use metrix_plan::{Assertion, Condition};
use regex::Regex;
use serde_json::Value;

use crate::extract::{Extractor, Response};

/// One compiled expectation.
pub(crate) enum Check {
    Status(u16),
    StatusIn(Vec<u16>),
    MaxLatency(Duration),
    ContentType(String),
    Selected {
        extractor: Extractor,
        condition: Compiled,
    },
}

/// A condition on a selected value, with its regular expression already built.
pub(crate) struct Compiled {
    exists: Option<bool>,
    equals: Option<Value>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    matches: Option<Regex>,
}

impl Check {
    pub fn compile(at: &str, assertion: &Assertion) -> Result<Self, String> {
        Ok(match assertion {
            Assertion::Status { status } => Self::Status(*status),
            Assertion::StatusIn { status_in } => {
                if status_in.is_empty() {
                    return Err(format!("{at}: status_in lists no statuses"));
                }
                Self::StatusIn(status_in.clone())
            }
            Assertion::MaxLatency { max_latency_ms } => {
                Self::MaxLatency(Duration::from_millis(*max_latency_ms))
            }
            Assertion::ContentType { content_type } => {
                if content_type.is_empty() {
                    return Err(format!("{at}: an empty content type"));
                }
                Self::ContentType(content_type.to_ascii_lowercase())
            }
            Assertion::Selected {
                selector,
                condition,
            } => {
                if condition.is_empty() {
                    // A selector with nothing asked of it is vacuously true, which
                    // is a plan error wearing the clothes of a passing test.
                    return Err(format!(
                        "{at}: a selector with no condition; say what must be true of it"
                    ));
                }
                Self::Selected {
                    extractor: Extractor::compile(at, selector)?,
                    condition: Compiled::new(at, condition)?,
                }
            }
        })
    }

    /// True when this assertion has to read the response body.
    pub fn reads_body(&self) -> bool {
        matches!(self, Self::Selected { extractor, .. } if !matches!(extractor, Extractor::Header(_)))
    }

    /// True when it has to read the response headers.
    pub fn reads_headers(&self) -> bool {
        match self {
            Self::ContentType(_) => true,
            Self::Selected { extractor, .. } => matches!(extractor, Extractor::Header(_)),
            _ => false,
        }
    }
}

impl Compiled {
    fn new(at: &str, condition: &Condition) -> Result<Self, String> {
        Ok(Self {
            exists: condition.exists,
            equals: condition.equals.clone(),
            min_length: condition.min_length,
            max_length: condition.max_length,
            matches: condition
                .matches
                .as_ref()
                .map(|pattern| {
                    Regex::new(pattern)
                        .map_err(|error| format!("{at}: invalid regular expression — {error}"))
                })
                .transpose()?,
        })
    }

    fn holds(&self, found: Option<&Selected>) -> bool {
        if self.exists == Some(false) {
            return found.is_none();
        }
        let Some(selected) = found else {
            // Everything else is a statement about a value, and there is no value.
            return false;
        };
        if self.exists == Some(true) && self.only_existence() {
            return true;
        }
        if let Some(expected) = &self.equals {
            if selected.text != render(expected) {
                return false;
            }
        }
        if let Some(least) = self.min_length {
            if selected.length < least {
                return false;
            }
        }
        if let Some(most) = self.max_length {
            if selected.length > most {
                return false;
            }
        }
        if let Some(pattern) = &self.matches {
            if !pattern.is_match(&selected.text) {
                return false;
            }
        }
        true
    }

    fn only_existence(&self) -> bool {
        self.equals.is_none()
            && self.min_length.is_none()
            && self.max_length.is_none()
            && self.matches.is_none()
    }
}

/// A selected value, and the length that means something for its shape.
pub(crate) struct Selected {
    pub text: String,
    /// Items for an array, entries for an object, characters for a string. A JSON
    /// array's length is what `min_length: 1` is asking about, and the length of the
    /// text it serialises to is not.
    pub length: usize,
}

/// Evaluate every assertion in order; the first that fails is the one reported.
///
/// In order and stopping at the first, because a call that asserts a 200 and then a
/// field inside the body has already said everything useful when the status was 500.
pub(crate) fn evaluate(
    checks: &[Check],
    status: Option<u16>,
    latency: Option<Duration>,
    response: Option<&Response<'_>>,
) -> Option<usize> {
    for (index, check) in checks.iter().enumerate() {
        let holds = match check {
            Check::Status(expected) => status == Some(*expected),
            Check::StatusIn(expected) => status.is_some_and(|got| expected.contains(&got)),
            Check::MaxLatency(limit) => latency.is_some_and(|took| took <= *limit),
            Check::ContentType(expected) => response.is_some_and(|response| {
                response
                    .headers
                    .get(hyper::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|got| {
                        // `application/json; charset=utf-8` satisfies
                        // `application/json`: the parameters are the server's
                        // business, and the type is what was asked about.
                        got.to_ascii_lowercase()
                            .split(';')
                            .next()
                            .is_some_and(|kind| kind.trim() == expected)
                    })
            }),
            Check::Selected {
                extractor,
                condition,
            } => {
                let found = response.and_then(|response| response.select(extractor));
                condition.holds(found.as_ref())
            }
        };
        if !holds {
            return Some(index);
        }
    }
    None
}

/// A JSON value as the text an assertion compares against, matching extraction.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
