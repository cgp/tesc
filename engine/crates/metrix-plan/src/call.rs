//! The **call** document: individual request definitions.
//!
//! A call knows how to make one request and how to judge the response. It knows
//! nothing about how often it runs or what runs before it — that is the mix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{Method, Selector};

/// One file of calls, keyed by call name.
pub type CallFile = BTreeMap<String, Call>;

/// A single request definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// One line of prose, for the read-only call inspector in the UI. The only field
    /// in the format that exists purely for a human reading it. Empty is allowed so a
    /// scratch bundle still runs; generated plans fill it from the OpenAPI summary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,

    pub method: Method,

    /// Path template, e.g. `/api/products/{{ pid }}`.
    pub path: String,

    /// Query parameters, kept separate from `path` so a generator can set them
    /// without string-splicing a URL.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Inline text, with `{{ }}` templating applied. A body a generator produces is
    /// declared in `generate`, because the hook returns the whole request and a
    /// `body` block that could set the path would be a field lying about what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    /// Build this request with a generator declared in the mix (§7.1). Whatever the
    /// hook returns replaces that part of the request; whatever it omits keeps what
    /// the call wrote here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<Generate>,

    /// Overrides the mix-level default for this call only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// What the response must look like. An empty list is legal and means the call is
    /// judged only by transport success, which is rarely what anyone wants.
    #[serde(default, rename = "assert", skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,

    /// Values pulled out of the response into the chain's variable scope.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extract: BTreeMap<String, Selector>,
}

/// A call handed to a generator declared in the mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    /// A name in the mix's `generators` block.
    pub generator: String,
    /// Passed through to the hook. Strings are templated like any other field, so a
    /// generator can be handed `{{ users.email }}` without knowing datasets exist.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, Value>,
}

/// A declarative, enumerable expectation. No expression language: every variant is a
/// shape an LLM can emit from a schema, and every failure names the specific field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Assertion {
    /// `{ "status": 200 }`
    Status { status: u16 },
    /// `{ "status_in": [201, 202] }`
    StatusIn { status_in: Vec<u16> },
    /// `{ "max_latency_ms": 300 }`
    MaxLatency { max_latency_ms: u64 },
    /// `{ "content_type": "application/xml" }` — asked for XML, got an HTML error page.
    ContentType { content_type: String },
    /// A condition on something selected out of the response, e.g.
    /// `{ "json": "$.items", "min_length": 1 }` or `{ "xpath": "/order/id", "exists": true }`.
    Selected {
        #[serde(flatten)]
        selector: Selector,
        #[serde(flatten)]
        condition: Condition,
    },
}

/// What must be true of a selected value.
// Flattened into `Assertion::Selected` alongside a selector, so it cannot deny
// unknown fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Condition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
}

impl Condition {
    /// True when no condition was actually stated, which is a plan error rather than
    /// a vacuous truth. Reported by validation in F0.3.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.exists.is_none()
            && self.equals.is_none()
            && self.min_length.is_none()
            && self.max_length.is_none()
            && self.matches.is_none()
    }
}
