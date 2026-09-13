//! Primitives shared by the three plan documents.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// A duration written the way a person writes one: `"30s"`, `"10m"`, `"1h30m"`.
///
/// Serializes back to the same spelling, so a round-trip through the API does not
/// rewrite `"1m"` as `"60s"` in a file someone is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dur(Duration);

impl Dur {
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    #[must_use]
    pub const fn as_secs_f64(self) -> f64 {
        self.0.as_secs_f64()
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl From<Dur> for Duration {
    fn from(d: Dur) -> Self {
        d.0
    }
}

/// Why a duration string could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected a duration like \"30s\", \"10m\" or \"1h30m\", got {input:?}: {reason}")]
pub struct DurParseError {
    pub input: String,
    pub reason: &'static str,
}

impl FromStr for Dur {
    type Err = DurParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fail = |reason| DurParseError {
            input: s.to_owned(),
            reason,
        };

        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(fail("it is empty"));
        }

        let mut total = Duration::ZERO;
        let mut digits = String::new();
        let mut saw_unit = false;

        for ch in trimmed.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                continue;
            }
            if digits.is_empty() {
                return Err(fail("a unit appears without a number before it"));
            }
            let value: u64 = digits
                .parse()
                .map_err(|_| fail("the number is too large"))?;
            digits.clear();
            saw_unit = true;

            let unit_secs = match ch {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                _ => return Err(fail("the unit must be one of s, m, h")),
            };
            total = total
                .checked_add(Duration::from_secs(
                    value
                        .checked_mul(unit_secs)
                        .ok_or_else(|| fail("the total is too large"))?,
                ))
                .ok_or_else(|| fail("the total is too large"))?;
        }

        if !digits.is_empty() {
            return Err(fail("the number has no unit; write 30s, not 30"));
        }
        if !saw_unit {
            return Err(fail("it has no units"));
        }
        Ok(Self(total))
    }
}

impl fmt::Display for Dur {
    /// Renders the largest whole units that divide evenly: 90s stays `90s`, 120s becomes `2m`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        if secs == 0 {
            return write!(f, "0s");
        }
        if secs % 3600 == 0 {
            write!(f, "{}h", secs / 3600)
        } else if secs % 60 == 0 {
            write!(f, "{}m", secs / 60)
        } else {
            write!(f, "{secs}s")
        }
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(de::Error::custom)
    }
}

/// A duration is a string in the schema, with the spelling documented in the pattern.
/// Written by hand because `Dur`'s serde implementation is hand-written.
impl schemars::JsonSchema for Dur {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": r"^(\d+[smh])+$",
            "description": "A duration such as \"30s\", \"10m\" or \"1h30m\". Units are required.",
            "examples": ["30s", "10m", "1h30m"],
        })
    }
}

/// HTTP methods the engine will issue. HTTP/1.1 and HTTP/2 only (see the contract document).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        };
        f.write_str(s)
    }
}

/// Where a response value is read from. The response content type picks the default
/// parser; naming an extractor type here overrides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    /// JSONPath, e.g. `$.items[0].id`
    Json(String),
    /// XPath, e.g. `/order/id/text()`
    Xpath(String),
    /// A response header name
    Header(String),
    /// A regular expression with one capture group
    Regex(String),
}
