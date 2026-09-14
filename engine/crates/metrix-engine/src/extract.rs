//! Pulling a value out of a response and into the chain's scope.
//!
//! Four extractors, because four cover what services actually answer with: JSONPath
//! for JSON, XPath for XML, a header, and a regular expression for everything that is
//! neither. The response's own `Content-Type` picks the default parser and naming an
//! extractor overrides it (design-engine §5).
//!
//! **The body is parsed at most once per response, however many extractors read it.**
//! A chain step with four JSONPath extractors parses one document, not four: parsing
//! is the expensive half of extraction and the generator is the thing under the most
//! pressure during a run.
//!
//! **Nothing here reports "not found" as an empty value.** A selector that matched
//! nothing leaves the variable unset, and the step that needed it fails naming it —
//! the alternative is a request built around an empty string, answered with a 404,
//! and read as the service being broken.

use std::sync::OnceLock;

use hyper::HeaderMap;
use metrix_plan::Selector;
use regex::Regex;
use serde_json::Value;
use serde_json_path::JsonPath;

/// A selector compiled once, so a bad path is a load-time error rather than a
/// per-request one. Regular expressions especially: compiling one per response would
/// cost more than the request it is reading.
pub(crate) enum Extractor {
    Json(JsonPath),
    Xpath(String),
    Header(String),
    Regex(Regex),
}

impl Extractor {
    pub fn compile(at: &str, selector: &Selector) -> Result<Self, String> {
        Ok(match selector {
            Selector::Json(path) => Self::Json(
                JsonPath::parse(path)
                    .map_err(|error| format!("{at}: invalid JSONPath — {error}"))?,
            ),
            Selector::Xpath(path) => {
                // Compiled per use rather than held: sxd's compiled form is not
                // `Send`, and a chain step moves across worker threads. Validated
                // here so a malformed path is still refused before the run.
                sxd_xpath::Factory::new()
                    .build(path)
                    .map_err(|error| format!("{at}: invalid XPath — {error}"))?
                    .ok_or_else(|| format!("{at}: empty XPath"))?;
                Self::Xpath(path.clone())
            }
            Selector::Header(name) => {
                if name.is_empty() {
                    return Err(format!("{at}: an empty header name"));
                }
                Self::Header(name.to_ascii_lowercase())
            }
            Selector::Regex(pattern) => {
                let compiled = Regex::new(pattern)
                    .map_err(|error| format!("{at}: invalid regular expression — {error}"))?;
                if compiled.captures_len() < 2 {
                    return Err(format!(
                        "{at}: the expression has no capture group, so there is nothing \
                         to extract"
                    ));
                }
                Self::Regex(compiled)
            }
        })
    }
}

/// One response, parsed lazily and at most once per representation.
pub(crate) struct Response<'a> {
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
    json: OnceLock<Option<Value>>,
    text: OnceLock<Option<&'a str>>,
}

impl<'a> Response<'a> {
    pub fn new(headers: &'a HeaderMap, body: &'a [u8]) -> Self {
        Self {
            headers,
            body,
            json: OnceLock::new(),
            text: OnceLock::new(),
        }
    }

    fn json(&self) -> Option<&Value> {
        self.json
            .get_or_init(|| serde_json::from_slice(self.body).ok())
            .as_ref()
    }

    fn text(&self) -> Option<&'a str> {
        *self
            .text
            .get_or_init(|| std::str::from_utf8(self.body).ok())
    }

    /// Read one value, or nothing. Nothing is an answer: see the module note.
    pub fn read(&self, extractor: &Extractor) -> Option<String> {
        match extractor {
            Extractor::Json(path) => path.query(self.json()?).first().map(stringify),
            Extractor::Xpath(path) => self.xpath(path),
            Extractor::Header(name) => self
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            Extractor::Regex(pattern) => pattern
                .captures(self.text()?)
                .and_then(|captures| captures.get(1))
                .map(|matched| matched.as_str().to_owned()),
        }
    }

    fn xpath(&self, path: &str) -> Option<String> {
        let package = sxd_document::parser::parse(self.text()?).ok()?;
        let document = package.as_document();
        let compiled = sxd_xpath::Factory::new().build(path).ok()??;
        let context = sxd_xpath::Context::new();
        let value = compiled.evaluate(&context, document.root()).ok()?;
        let text = value.string();
        // An XPath that matched nothing evaluates to the empty string, which is the
        // one case this must not report as a captured value.
        (!text.is_empty()).then_some(text)
    }
}

/// A JSON value as the text a request will carry.
///
/// Strings are unquoted, because `{{ id }}` in a path wants `A-1000` and not
/// `"A-1000"`. Everything else keeps its JSON spelling, which is what a body
/// templating it back in would need.
fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// `Extractor` holds compiled regular expressions and paths and is deliberately
    /// not `Debug`; unwrapping it in a test would print one.
    fn ok(result: Result<Extractor, String>) -> Extractor {
        match result {
            Ok(extractor) => extractor,
            Err(error) => panic!("{error}"),
        }
    }

    fn read(selector: Selector, body: &str, head: &[(&str, &str)]) -> Option<String> {
        let extractor = ok(Extractor::compile("at", &selector));
        let map = headers(head);
        Response::new(&map, body.as_bytes()).read(&extractor)
    }

    #[test]
    fn a_json_string_arrives_without_its_quotes() {
        // `{{ id }}` in a path wants A-1000, not "A-1000".
        let body = r#"{"items":[{"id":"A-1000","qty":2}]}"#;
        assert_eq!(
            read(Selector::Json("$.items[0].id".into()), body, &[]),
            Some("A-1000".into())
        );
        assert_eq!(
            read(Selector::Json("$.items[0].qty".into()), body, &[]),
            Some("2".into())
        );
    }

    #[test]
    fn a_json_path_that_matches_nothing_captures_nothing() {
        let body = r#"{"items":[]}"#;
        assert_eq!(
            read(Selector::Json("$.items[0].id".into()), body, &[]),
            None
        );
    }

    #[test]
    fn xpath_reads_an_element_and_an_attribute() {
        let body = r#"<order id="A-1"><total>42</total></order>"#;
        assert_eq!(
            read(Selector::Xpath("/order/total/text()".into()), body, &[]),
            Some("42".into())
        );
        assert_eq!(
            read(Selector::Xpath("/order/@id".into()), body, &[]),
            Some("A-1".into())
        );
    }

    #[test]
    fn an_xpath_matching_nothing_is_nothing_rather_than_an_empty_capture() {
        let body = "<order><total>42</total></order>";
        assert_eq!(
            read(Selector::Xpath("/order/missing".into()), body, &[]),
            None
        );
    }

    #[test]
    fn a_header_is_read_whatever_case_it_was_sent_in() {
        assert_eq!(
            read(
                Selector::Header("Location".into()),
                "",
                &[("location", "/orders/7")]
            ),
            Some("/orders/7".into())
        );
    }

    #[test]
    fn a_regex_yields_its_capture_group() {
        assert_eq!(
            read(Selector::Regex("id=([0-9]+)".into()), "x id=42 y", &[]),
            Some("42".into())
        );
    }

    #[test]
    fn a_regex_with_no_capture_group_is_refused_at_compile_time() {
        let error = Extractor::compile("at", &Selector::Regex("[0-9]+".into()))
            .err()
            .expect("a pattern with no group is refused");
        assert!(error.contains("capture group"), "{error}");
    }

    #[test]
    fn a_body_that_is_not_what_the_selector_expects_captures_nothing() {
        // An HTML error page where JSON was expected. Nothing captured, and the step
        // that needed it fails naming the variable rather than sending an empty one.
        assert_eq!(
            read(Selector::Json("$.id".into()), "<html>nope</html>", &[]),
            None
        );
        assert_eq!(read(Selector::Xpath("/a".into()), "{\"a\":1}", &[]), None);
    }

    #[test]
    fn a_malformed_selector_is_a_load_time_error() {
        for selector in [
            Selector::Json("$[".into()),
            Selector::Xpath("///".into()),
            Selector::Header(String::new()),
        ] {
            assert!(
                Extractor::compile("at", &selector).is_err(),
                "accepted {selector:?}"
            );
        }
    }

    #[test]
    fn one_response_is_parsed_once_however_many_extractors_read_it() {
        let body = r#"{"a":1,"b":2}"#;
        let map = headers(&[]);
        let response = Response::new(&map, body.as_bytes());
        let first = ok(Extractor::compile("at", &Selector::Json("$.a".into())));
        let second = ok(Extractor::compile("at", &Selector::Json("$.b".into())));
        assert_eq!(response.read(&first), Some("1".into()));
        assert_eq!(response.read(&second), Some("2".into()));
        // The cache is the proof: a second read found the document already there.
        assert!(response.json.get().is_some());
    }
}
