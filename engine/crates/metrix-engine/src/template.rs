//! `{{ name }}` in a request, and the scope it is read from.
//!
//! A chain is one virtual user with its own variable scope (design-engine §5): a step
//! extracts a value out of its response, and a later step in the same iteration sends
//! it. Nothing is shared between iterations, because two virtual users are two
//! different people as far as the service is concerned.
//!
//! **A variable with nothing behind it fails the step.** Substituting an empty string
//! would send `/api/orders/` to a service that expects an id, which answers 404 —
//! and the run would report a service returning 404s rather than a plan referring to
//! something it never captured. The failure has to name the variable.

use std::collections::BTreeMap;
use std::fmt::Write;

/// One iteration's variables. Cleared between iterations rather than reallocated:
/// a chain runs thousands of times and the names do not change.
pub(crate) type Scope = BTreeMap<String, String>;

/// Why a request could not be built from what the chain had captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unbound(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Variable(String),
}

/// A piece of a request that may carry variables, compiled once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    segments: Vec<Segment>,
    /// The whole text, when there are no variables in it. Every request in a run
    /// with no chaining renders exactly this, so the common path is a clone of one
    /// string rather than a walk over segments.
    fixed: Option<String>,
}

impl Template {
    /// Compile one templated string. `at` names the field, for the error.
    pub fn parse(at: &str, text: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find("{{") {
            let (literal, after) = rest.split_at(open);
            if !literal.is_empty() {
                segments.push(Segment::Literal(literal.to_owned()));
            }
            let body = &after[2..];
            let close = body
                .find("}}")
                .ok_or_else(|| format!("{at}: a {{{{ is never closed"))?;
            let name = body[..close].trim();
            require_name(at, name)?;
            segments.push(Segment::Variable(name.to_owned()));
            rest = &body[close + 2..];
        }
        if let Some(stray) = rest.find("}}") {
            let _ = stray;
            return Err(format!("{at}: a }}}} with no {{{{ before it"));
        }
        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_owned()));
        }

        let fixed = segments
            .iter()
            .all(|segment| matches!(segment, Segment::Literal(_)))
            .then(|| text.to_owned());
        Ok(Self { segments, fixed })
    }

    /// Every variable this template reads, in the order it reads them.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Variable(name) => Some(name.as_str()),
            Segment::Literal(_) => None,
        })
    }

    /// Fill it in from one iteration's scope.
    pub fn render(&self, scope: &Scope) -> Result<String, Unbound> {
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Variable(name) => match scope.get(name) {
                    Some(value) => out.push_str(value),
                    None => return Err(Unbound(name.clone())),
                },
            }
        }
        Ok(out)
    }

    /// Fill it in, percent-encoding each substituted value for a query string.
    ///
    /// Encoded on the way in rather than over the finished string: a captured value
    /// containing `&` would otherwise become a second parameter, and a captured value
    /// containing `%` would be double-encoded if the literal parts were encoded too.
    pub fn render_query(&self, scope: &Scope) -> Result<String, Unbound> {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(&encode(text)),
                Segment::Variable(name) => match scope.get(name) {
                    Some(value) => out.push_str(&encode(value)),
                    None => return Err(Unbound(name.clone())),
                },
            }
        }
        Ok(out)
    }
}

/// What may stand between `{{` and `}}` today.
///
/// Bare names only. `{{ pick('a','b') }}` and `{{ rand(1,20) }}` are generation
/// rather than substitution, and arrive with the datasets they belong to; refusing
/// them by name beats evaluating them as a variable called `pick('a','b')` and
/// reporting it as unbound.
fn require_name(at: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{at}: an empty {{{{ }}}}"));
    }
    if name.contains('(') {
        return Err(format!(
            "{at}: {name:?} is an expression; inline generation is not implemented (B3.5)"
        ));
    }
    let shape = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    if !shape {
        return Err(format!(
            "{at}: {name:?} is not a variable name; letters, digits, dot, dash and \
             underscore"
        ));
    }
    Ok(())
}

pub(crate) fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(pairs: &[(&str, &str)]) -> Scope {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_template_with_no_variables_renders_itself() {
        let template = Template::parse("at", "/api/products").unwrap();
        assert_eq!(template.variables().count(), 0);
        assert_eq!(template.render(&scope(&[])).unwrap(), "/api/products");
    }

    #[test]
    fn surrounding_text_is_kept_exactly() {
        let template = Template::parse("at", "/api/orders/{{ id }}/items").unwrap();
        assert_eq!(template.variables().count(), 1);
        assert_eq!(
            template.render(&scope(&[("id", "A-1000")])).unwrap(),
            "/api/orders/A-1000/items"
        );
    }

    #[test]
    fn a_variable_with_nothing_behind_it_names_itself() {
        let template = Template::parse("at", "/orders/{{ order_id }}").unwrap();
        // Not an empty string: `/orders/` is a different request, and the service
        // answering it 404 would be reported as the service's problem.
        assert_eq!(
            template.render(&scope(&[])),
            Err(Unbound("order_id".into()))
        );
    }

    #[test]
    fn a_captured_value_cannot_smuggle_a_second_query_parameter() {
        let template = Template::parse("at", "{{ term }}").unwrap();
        assert_eq!(
            template.render_query(&scope(&[("term", "a&b=c")])).unwrap(),
            "a%26b%3Dc"
        );
    }

    #[test]
    fn an_unclosed_or_stray_brace_is_refused_at_compile_time() {
        assert!(Template::parse("at", "/a/{{ id").is_err());
        assert!(Template::parse("at", "/a/id }}").is_err());
        assert!(Template::parse("at", "/a/{{}}").is_err());
    }

    #[test]
    fn an_expression_says_which_step_brings_it() {
        let error = Template::parse("at", "{{ pick('a','b') }}").unwrap_err();
        assert!(error.contains("B3.5"), "{error}");
    }

    #[test]
    fn the_variables_a_template_reads_are_listed_in_order() {
        let template = Template::parse("at", "{{ a }}/x/{{ b }}").unwrap();
        assert_eq!(template.variables().collect::<Vec<_>>(), ["a", "b"]);
    }
}
