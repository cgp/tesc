//! The **targets** document: the boxes to run against.
//!
//! Produced by the API from a resolved profile, or written by hand. The engine
//! receives concrete addresses and no cloud context. A list of more than one target
//! is a sweep; one target is the degenerate case.

use serde::{Deserialize, Serialize};

use crate::common::Dur;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    #[serde(default)]
    pub order: TargetOrder,

    /// Idle between targets so shared dependencies settle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Dur>,

    pub list: Vec<Target>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetOrder {
    #[default]
    AsResolved,
    /// Decouples results from sweep position: the first target pays cold-cache costs
    /// on shared dependencies that the rest do not.
    Shuffle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Stable identity for charts and comparison — a task id, instance id, or hostname.
    pub id: String,

    /// Where to send traffic: `host:port` or `ip:port`.
    pub address: String,

    /// Sent as `Host`, and used for TLS SNI and certificate verification when going
    /// direct to a container by IP. Most services route or vhost on it; a raw IP gets
    /// a 404 or a default backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_header: Option<String>,

    #[serde(default)]
    pub tls: Tls,

    /// Free-form attributes carried from the resolved inventory — instance type, AZ,
    /// image digest, task definition revision. Opaque to the engine; echoed into the
    /// output so comparison can explain an outlier.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub attributes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    #[serde(default)]
    pub enabled: bool,
    /// Overrides `host_header` for SNI when they must differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sni: Option<String>,
    /// Raises a run annotation when used.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}
