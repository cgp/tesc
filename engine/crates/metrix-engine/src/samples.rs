//! Keeping the first few failures in full, and keeping the secrets out of them.
//!
//! **Per error class, not overall** (design-engine §9.3). One flood of
//! connection-refused would otherwise evict the single 500 that actually explains the
//! problem, and the 500 is what somebody reading the run needs.
//!
//! **The first N rather than a random N**, deliberately: the first failures are the
//! ones that show what changed at onset, and they cost nothing to collect. After N, a
//! class only increments a counter — which is also why the budget is checked before a
//! response body is kept at all, so a run that has already seen its ten connection
//! resets stops buffering bodies for them.
//!
//! **Secrets are redacted at capture, not at display.** Once a body is written to a
//! file the redaction has already had to happen; anything else is a promise about who
//! reads the file. Two things are taken out: header and field names the plan named in
//! `capture.redact`, and the literal value of every `{{ secret.X }}` the plan
//! resolved, wherever it appears.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

use hyper::HeaderMap;
use metrix_metrics::events::{ErrorClass, SampledRequest, SampledResponse};

/// What a redacted value is replaced with. A marker rather than a blank, so a reader
/// can tell a redacted header from one the service did not send.
const REDACTED: &str = "[redacted]";

/// How many classes there are to budget for, so the array is indexed rather than
/// searched. Adding an `ErrorClass` makes `index` below non-exhaustive, which is a
/// compile error; the test at the bottom is what keeps this number honest with it.
const CLASSES: usize = 15;

/// The per-class budget, and what must never be written down.
pub(crate) struct Samples {
    per_class: u32,
    kept: [AtomicU32; CLASSES],
    /// Remaining across every class. Read once per errored response to decide whether
    /// keeping its body is worth anything.
    remaining: AtomicU32,
    /// Header and field names the plan asked to have taken out, lowercased once.
    redact: Vec<String>,
    body_max: usize,
}

impl Samples {
    pub fn new(per_class: u32, redact: &[String], body_max: usize) -> Self {
        Self {
            per_class,
            kept: std::array::from_fn(|_| AtomicU32::new(0)),
            remaining: AtomicU32::new(per_class.saturating_mul(CLASSES as u32)),
            redact: redact
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
            body_max,
        }
    }

    /// True while some class could still use a response body.
    ///
    /// Checked before the body is buffered rather than after: a broken run produces
    /// errors by the thousand, and the point of a ceiling is not to hold them all.
    pub fn wants_bodies(&self) -> bool {
        self.per_class > 0 && self.remaining.load(Ordering::Relaxed) > 0
    }

    /// Claim a slot for this class, or `None` when the class is full.
    pub fn claim(&self, class: ErrorClass) -> Option<u32> {
        if self.per_class == 0 {
            return None;
        }
        let slot = &self.kept[index(class)];
        let taken = slot
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |kept| {
                (kept < self.per_class).then_some(kept + 1)
            })
            .ok()?;
        self.remaining.fetch_sub(1, Ordering::Relaxed);
        Some(taken + 1)
    }

    /// The request as sent, with the secrets taken out.
    pub fn request(
        &self,
        method: &str,
        target: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> SampledRequest {
        let (body, truncated) = self.body(body);
        SampledRequest {
            method: method.to_owned(),
            target: redact_text(target),
            headers: self.headers(headers),
            body,
            body_truncated: truncated,
        }
    }

    pub fn response(&self, status: u16, headers: &HeaderMap, body: &[u8]) -> SampledResponse {
        let (body, truncated) = self.body(body);
        SampledResponse {
            status,
            headers: self.headers(headers),
            body,
            body_truncated: truncated,
        }
    }

    fn headers(&self, headers: &HeaderMap) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, value) in headers {
            let text = value.to_str().unwrap_or("[not text]");
            let kept = if self.redact.iter().any(|hidden| hidden == name.as_str()) {
                REDACTED.to_owned()
            } else {
                redact_text(text)
            };
            // Appended rather than replaced: a response can carry several `Set-Cookie`
            // lines, and keeping one of them would be a sample that is not the request.
            out.entry(name.as_str().to_owned())
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(&kept);
                })
                .or_insert(kept);
        }
        out
    }

    /// The body as text, cut at the capture ceiling and scrubbed.
    fn body(&self, bytes: &[u8]) -> (Option<String>, bool) {
        if bytes.is_empty() {
            return (None, false);
        }
        let truncated = bytes.len() > self.body_max;
        let kept = &bytes[..bytes.len().min(self.body_max)];
        let text = String::from_utf8_lossy(kept);
        (Some(self.scrub(&text)), truncated)
    }

    /// Take the named fields out of a body, and the secrets out of anything.
    ///
    /// Field matching is textual — `"password": "..."` and `password=...` — rather
    /// than by parsing, because a sample is kept precisely when something went wrong
    /// and a body that will not parse is exactly the case worth keeping.
    fn scrub(&self, text: &str) -> String {
        let mut out = redact_text(text);
        for name in &self.redact {
            out = hide_field(&out, name);
        }
        out
    }
}

/// Which class this outcome would be sampled under, or nothing when it succeeded.
///
/// A status the plan tolerates is not a failure: a chain asserting 401 is a
/// deliberately-failing flow, and a sample kept for it would be a sample of the plan
/// working.
pub(crate) fn classify(
    observation: &crate::http::Observation,
    verdict: Option<crate::chain::Verdict>,
) -> Option<ErrorClass> {
    if let Some(failure) = observation.error {
        return Some(match failure {
            crate::Failure::Dns => ErrorClass::DnsFailure,
            crate::Failure::Connect => ErrorClass::ConnectionRefused,
            crate::Failure::LocalResource => ErrorClass::Generation,
            crate::Failure::Tls => ErrorClass::TlsFailure,
            crate::Failure::Timeout => ErrorClass::ReadTimeout,
            crate::Failure::Protocol => ErrorClass::UnexpectedEof,
            crate::Failure::Send | crate::Failure::Body => ErrorClass::ConnectionReset,
        });
    }
    match verdict {
        Some(crate::chain::Verdict::Assertion(_)) | Some(crate::chain::Verdict::Unfinished) => {
            Some(ErrorClass::Assertion)
        }
        Some(crate::chain::Verdict::Unauthorized) => Some(ErrorClass::Unauthorized),
        // A 5xx nobody asserted against is still the thing somebody will want to read,
        // and it is the case the per-class budget exists to protect.
        None if observation.status.is_some_and(|status| status >= 500) => {
            Some(ErrorClass::HttpStatus)
        }
        None => None,
    }
}

/// Replace every resolved secret wherever it appears.
fn redact_text(text: &str) -> String {
    let mut out = text.to_owned();
    for secret in crate::template::resolved_secrets() {
        if !secret.is_empty() && out.contains(&secret) {
            out = out.replace(&secret, REDACTED);
        }
    }
    out
}

/// Hide the value that follows `name` in a JSON object or a form body.
fn hide_field(text: &str, name: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lowered = text.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(found) = lowered[cursor..].find(name) {
        let start = cursor + found;
        let after = start + name.len();
        // `"password"` in JSON and `password` in a form body: what follows decides.
        let rest = &text[after..];
        let Some(offset) = value_at(rest) else {
            out.push_str(&text[cursor..after]);
            cursor = after;
            continue;
        };
        out.push_str(&text[cursor..after + offset.0]);
        out.push_str(REDACTED);
        cursor = after + offset.1;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Where the value after a field name starts and ends, if this looks like a field.
fn value_at(rest: &str) -> Option<(usize, usize)> {
    let bytes = rest.as_bytes();
    let mut index = 0;
    // `"` closing a JSON key, then `:`, or `=` for a form field.
    if bytes.first() == Some(&b'"') {
        index += 1;
    }
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    match bytes.get(index) {
        Some(b':') | Some(b'=') => index += 1,
        _ => return None,
    }
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    if bytes.get(index) == Some(&b'"') {
        let start = index + 1;
        let end = rest[start..].find('"').map(|at| start + at)?;
        return Some((start, end));
    }
    let start = index;
    let end = rest[start..]
        .find(['&', ',', '}', '\n', ' '])
        .map_or(rest.len(), |at| start + at);
    (end > start).then_some((start, end))
}

/// Which slot in the budget this class holds.
fn index(class: ErrorClass) -> usize {
    match class {
        ErrorClass::DnsFailure => 0,
        ErrorClass::ConnectionRefused => 1,
        ErrorClass::ConnectTimeout => 2,
        ErrorClass::ReadTimeout => 3,
        ErrorClass::TlsFailure => 4,
        ErrorClass::ConnectionReset => 5,
        ErrorClass::UnexpectedEof => 6,
        ErrorClass::HttpStatus => 7,
        ErrorClass::ContentTypeMismatch => 8,
        ErrorClass::SchemaValidation => 9,
        ErrorClass::Assertion => 10,
        ErrorClass::Extraction => 11,
        ErrorClass::Generation => 12,
        ErrorClass::Unauthorized => 13,
        ErrorClass::Other => 14,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Samples {
        Samples::new(2, &["Authorization".into(), "password".into()], 64 * 1024)
    }

    #[test]
    fn a_class_keeps_its_own_first_few() {
        let samples = samples();
        assert_eq!(samples.claim(ErrorClass::HttpStatus), Some(1));
        assert_eq!(samples.claim(ErrorClass::HttpStatus), Some(2));
        // Full, and the flood of resets that follows has not touched it.
        assert_eq!(samples.claim(ErrorClass::HttpStatus), None);
        assert_eq!(samples.claim(ErrorClass::ConnectionReset), Some(1));
    }

    /// Every class there is, so the budget array and `index` cannot drift apart.
    const EVERY: [ErrorClass; CLASSES] = [
        ErrorClass::DnsFailure,
        ErrorClass::ConnectionRefused,
        ErrorClass::ConnectTimeout,
        ErrorClass::ReadTimeout,
        ErrorClass::TlsFailure,
        ErrorClass::ConnectionReset,
        ErrorClass::UnexpectedEof,
        ErrorClass::HttpStatus,
        ErrorClass::ContentTypeMismatch,
        ErrorClass::SchemaValidation,
        ErrorClass::Assertion,
        ErrorClass::Extraction,
        ErrorClass::Generation,
        ErrorClass::Unauthorized,
        ErrorClass::Other,
    ];

    #[test]
    fn bodies_stop_being_kept_once_nothing_wants_them() {
        let samples = Samples::new(1, &[], 1024);
        assert!(samples.wants_bodies());
        for class in EVERY {
            assert_eq!(samples.claim(class), Some(1), "{class:?}");
        }
        // A broken run produces errors by the thousand; the ceiling exists so the
        // generator does not hold them all.
        assert!(!samples.wants_bodies());
    }

    #[test]
    fn keeping_nothing_is_a_setting_and_not_a_special_case() {
        let samples = Samples::new(0, &[], 1024);
        assert!(!samples.wants_bodies());
        assert_eq!(samples.claim(ErrorClass::HttpStatus), None);
    }

    #[test]
    fn a_named_header_is_taken_out_rather_than_left_for_the_reader() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer s3cr3t".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let kept = samples().headers(&headers);
        assert_eq!(kept["authorization"], REDACTED);
        // And nothing else is touched: a sample with every header hidden explains
        // nothing.
        assert_eq!(kept["content-type"], "application/json");
    }

    #[test]
    fn every_set_cookie_line_is_kept_rather_than_the_last_one() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());
        assert_eq!(samples().headers(&headers)["set-cookie"], "a=1, b=2");
    }

    #[test]
    fn a_named_field_is_taken_out_of_a_body_in_either_shape() {
        let samples = samples();
        assert_eq!(
            samples.scrub(r#"{"user":"ada","password":"hunter2"}"#),
            r#"{"user":"ada","password":"[redacted]"}"#
        );
        assert_eq!(
            samples.scrub("grant_type=password&password=hunter2&scope=x"),
            "grant_type=password&password=[redacted]&scope=x"
        );
    }

    #[test]
    fn a_body_that_will_not_parse_is_still_scrubbed() {
        // The case a sample is kept for: something went wrong, and the body is half a
        // document. Matching textually rather than by parsing is what makes this work.
        let scrubbed = samples().scrub(r#"{"password": "hunter2", "cart": [1,2"#);
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
    }

    #[test]
    fn a_body_is_cut_at_the_ceiling_and_says_so() {
        let samples = Samples::new(1, &[], 8);
        let (body, truncated) = samples.body(b"0123456789");
        assert_eq!(body.as_deref(), Some("01234567"));
        assert!(truncated);
        let (whole, cut) = samples.body(b"0123");
        assert_eq!(whole.as_deref(), Some("0123"));
        assert!(!cut);
    }
}
