//! The `auth` block.
//!
//! A first-class part of the mix rather than a hand-rolled chain step, because token
//! acquisition is infrastructure for the test rather than the thing being measured.
//! Auth traffic is excluded from load metrics; refresh is single-flight.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::call::Call;
use crate::common::Selector;

// No `deny_unknown_fields` here: it is incompatible with `flatten`, which would
// report the flattened mode keys as unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Auth {
    #[serde(flatten)]
    pub mode: AuthMode,

    /// How the credential is attached to each request.
    pub inject: Inject,

    #[serde(default)]
    pub refresh: Refresh,

    #[serde(default)]
    pub identity: Identity,
}

/// Secrets are written as `{{ env.NAME }}` or `{{ secret.NAME }}` and never stored
/// literally — plans are machine-generated and end up in repositories.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthMode {
    None,
    Basic {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
    OauthClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    OauthPassword {
        token_url: String,
        client_id: String,
        username: String,
        password: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    /// Bespoke auth: a full request plus extractors, reusing the chaining machinery.
    LoginRequest {
        request: Box<Call>,
        extract: BTreeMap<String, Selector>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    pub header: String,
    /// e.g. `"Bearer {{ token }}"`
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    #[serde(default)]
    pub strategy: RefreshStrategy,
    /// Refresh this far before expiry.
    #[serde(default = "default_margin_s")]
    pub margin_s: u64,
    #[serde(default)]
    pub on_401: On401,
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            strategy: RefreshStrategy::default(),
            margin_s: default_margin_s(),
            on_401: On401::default(),
        }
    }
}

const fn default_margin_s() -> u64 {
    30
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStrategy {
    #[default]
    ExpiresInMargin,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum On401 {
    /// Refresh once, single-flight, and retry. Without single-flight, 200 virtual
    /// users hitting a 401 together send 200 refreshes and the spike reads as the
    /// target degrading.
    #[default]
    RefreshOnce,
    Fail,
    Ignore,
}

/// Who the load is authenticated as.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    /// One token for all virtual users. Cheapest, but hides per-user rate limiting
    /// and per-user cache locality entirely.
    #[default]
    Shared,
    /// Each virtual user holds its own token.
    PerVu,
    /// A token per row of a dataset: realistic multi-tenant load.
    #[serde(untagged)]
    FromDataset(String),
}
