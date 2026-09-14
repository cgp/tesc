//! `{{ ... }}` in a request, and where each kind of value comes from.
//!
//! A chain is one virtual user with its own variable scope (design-engine §5): a step
//! extracts a value out of its response, and a later step in the same iteration sends
//! it. Nothing is shared between iterations, because two virtual users are two
//! different people as far as the service is concerned.
//!
//! Three things can stand between the braces, and they are told apart by shape rather
//! than by a sigil:
//!
//! | written | is | resolved |
//! |---|---|---|
//! | `{{ order_id }}` | a chain variable | from the iteration's scope |
//! | `{{ users.email }}` | a dataset field | at load, to a row and a column |
//! | `{{ uuid() }}` | a generator call | from the iteration's seeded stream |
//!
//! **The language is deliberately tiny** (§4.5) — substitution, a row lookup, and a
//! fixed function set. Anything more is a generator's job (§7), and keeping it this
//! small is what lets every reference in a plan be checked before the run starts.
//!
//! **A variable with nothing behind it fails the step.** Substituting an empty string
//! would send `/api/orders/` to a service that expects an id, which answers 404 —
//! and the run would report a service returning 404s rather than a plan referring to
//! something it never captured. The failure has to name the variable.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::dataset::Datasets;
use crate::random::Rng;

/// One iteration's variables. Cleared between iterations rather than reallocated:
/// a chain runs thousands of times and the names do not change.
pub(crate) type Scope = BTreeMap<String, String>;

/// Why a request could not be built from what the chain had captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unbound(pub String);

/// Everything one iteration renders its requests from.
///
/// Carried rather than looked up: the RNG is a stream, and every step of an iteration
/// draws from the same one in order, so a replay of the plan produces the same
/// requests in the same order.
pub(crate) struct Values<'a> {
    pub scope: &'a Scope,
    pub datasets: &'a Datasets,
    pub seed: u64,
    pub iteration: u64,
    pub rng: Rng,
}

impl<'a> Values<'a> {
    pub fn new(scope: &'a Scope, datasets: &'a Datasets, seed: u64, iteration: u64) -> Self {
        Self {
            scope,
            datasets,
            seed,
            iteration,
            rng: Rng::seeded(seed, iteration),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Variable(String),
    /// Resolved at load to the dataset's position and the column's, so rendering is
    /// two index operations rather than two map lookups.
    Field {
        dataset: usize,
        column: usize,
    },
    Call(Function),
}

/// The fixed function set (§4.5). Every one of them is a value a plan cannot hold
/// literally because it has to differ per iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Function {
    /// `rand(1, 20)` — an integer, both ends included.
    Rand { low: i64, high: i64 },
    /// `uuid()` — a version 4 UUID from the iteration's stream, so it is unique
    /// within the run and the same on a replay of it.
    Uuid,
    /// `now()`, `now('unix')`, `now('unix_ms')`.
    Now(Stamp),
    /// `seq()` — which iteration of the run this is, counting from one. Stable within
    /// an iteration: two steps of one chain that both send it send the same number,
    /// which is what makes it usable as an order reference across a chain.
    Seq,
    /// `pick('a', 'b')` — one of the listed values.
    Pick(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Iso,
    Unix,
    UnixMs,
}

/// A piece of a request that may carry variables, compiled once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    segments: Vec<Segment>,
    /// The whole text, when nothing in it varies. Every request in a run with no
    /// chaining renders exactly this, so the common path is a clone of one string
    /// rather than a walk over segments.
    fixed: Option<String>,
}

impl Template {
    /// Compile one templated string. `at` names the field, for the error.
    pub fn parse(at: &str, text: &str, datasets: &Datasets) -> Result<Self, String> {
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
            segments.push(reference(at, body[..close].trim(), datasets)?);
            rest = &body[close + 2..];
        }
        if rest.contains("}}") {
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

    /// Every chain variable this template reads, in the order it reads them.
    ///
    /// Only the variables: a dataset field and a generator call are answered without
    /// the scope, so an earlier step does not have to have produced them.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Variable(name) => Some(name.as_str()),
            _ => None,
        })
    }

    /// Every dataset this template reads.
    pub fn datasets(&self) -> impl Iterator<Item = usize> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Field { dataset, .. } => Some(*dataset),
            _ => None,
        })
    }

    /// True when this renders the same text on every iteration.
    pub fn is_fixed(&self) -> bool {
        self.fixed.is_some()
    }

    /// Fill it in from one iteration's values.
    pub fn render(&self, values: &mut Values<'_>) -> Result<String, Unbound> {
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                other => out.push_str(&resolve(other, values)?),
            }
        }
        Ok(out)
    }

    /// Fill it in, percent-encoding each substituted value for a query string.
    ///
    /// Encoded on the way in rather than over the finished string: a captured value
    /// containing `&` would otherwise become a second parameter, and a captured value
    /// containing `%` would be double-encoded if the literal parts were encoded too.
    pub fn render_query(&self, values: &mut Values<'_>) -> Result<String, Unbound> {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(&encode(text)),
                other => out.push_str(&encode(&resolve(other, values)?)),
            }
        }
        Ok(out)
    }
}

/// One non-literal segment, as the text it stands for.
fn resolve(segment: &Segment, values: &mut Values<'_>) -> Result<String, Unbound> {
    Ok(match segment {
        Segment::Literal(text) => text.clone(),
        Segment::Variable(name) => match values.scope.get(name) {
            Some(value) => value.clone(),
            None => return Err(Unbound(name.clone())),
        },
        Segment::Field { dataset, column } => {
            let set = values.datasets.get(*dataset);
            set.field(set.row(values.iteration, values.seed), *column)
                .to_owned()
        }
        Segment::Call(function) => call(function, values),
    })
}

fn call(function: &Function, values: &mut Values<'_>) -> String {
    match function {
        Function::Rand { low, high } => values.rng.in_range(*low, *high).to_string(),
        Function::Uuid => values.rng.uuid(),
        Function::Now(Stamp::Iso) => chrono::Utc::now().to_rfc3339_opts(
            chrono::SecondsFormat::Millis,
            // `Z` rather than `+00:00`: it is what a service parsing ISO-8601 is
            // likeliest to have been tested against.
            true,
        ),
        Function::Now(Stamp::Unix) => chrono::Utc::now().timestamp().to_string(),
        Function::Now(Stamp::UnixMs) => chrono::Utc::now().timestamp_millis().to_string(),
        Function::Seq => (values.iteration + 1).to_string(),
        Function::Pick(options) => {
            let index = values.rng.in_range(0, options.len() as i64 - 1) as usize;
            options[index].clone()
        }
    }
}

/// What stands between `{{` and `}}`: a variable, a dataset field, or a call.
fn reference(at: &str, text: &str, datasets: &Datasets) -> Result<Segment, String> {
    if text.is_empty() {
        return Err(format!("{at}: an empty {{{{ }}}}"));
    }
    if text.contains('(') || text.ends_with(')') {
        return function(at, text);
    }
    let shape = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    if !shape {
        return Err(format!(
            "{at}: {text:?} is not a variable name, a dataset field or a call"
        ));
    }
    if let Some(value) = ambient(at, text)? {
        return Ok(Segment::Literal(value));
    }
    match datasets.resolve(at, text)? {
        Some((dataset, column)) => Ok(Segment::Field { dataset, column }),
        None => Ok(Segment::Variable(text.to_owned())),
    }
}

/// `{{ env.NAME }}` and `{{ secret.NAME }}`, read from the process environment.
///
/// **Secrets never live in the plan** (design-engine §6.1): plans are machine-authored
/// and end up committed, so a credential is handed to the process instead. `secret.`
/// reads `METRIX_SECRET_<NAME>`, which keeps the two apart in the environment as well
/// as in the plan — a bundle exported to a bare load box then says exactly which
/// credentials it needs, by name, without carrying any of them.
///
/// Resolved once, here, into a literal. A value that does not change for the life of
/// the run should not be looked up per request, and a plan whose credential is simply
/// absent should fail before it sends anything rather than failing every request.
fn ambient(at: &str, text: &str) -> Result<Option<String>, String> {
    let (kind, name) = match text.split_once('.') {
        Some(("env", name)) => ("env", name.to_owned()),
        Some(("secret", name)) => ("secret", format!("METRIX_SECRET_{name}")),
        _ => return Ok(None),
    };
    let bare = name.trim_start_matches("METRIX_SECRET_");
    require_env_name(at, kind, bare)?;
    match std::env::var(&name) {
        Ok(value) => {
            if kind == "secret" {
                remember_secret(&value);
            }
            Ok(Some(value))
        }
        Err(_) => Err(format!(
            "{at}: {{{{ {kind}.{bare} }}}} is not set; the engine reads it from {name} in              its own environment, because a credential in a plan is a credential in a              repository"
        )),
    }
}

/// Every `{{ secret.X }}` value this process has resolved.
///
/// A process-wide set rather than something threaded through every `Template::parse`
/// call: which credentials this process was handed is a property of the process, and
/// there is exactly one place that reads it — capture, where a secret must never
/// reach a file (§6.1). Knowing the literal values is what makes that a mechanism
/// rather than a promise about which fields somebody remembered to name.
static SECRETS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn remember_secret(value: &str) {
    if value.is_empty() {
        return;
    }
    if let Ok(mut known) = SECRETS.lock()
        && !known.iter().any(|held| held == value)
    {
        known.push(value.to_owned());
    }
}

/// The secrets to take out of anything written down.
pub(crate) fn resolved_secrets() -> Vec<String> {
    SECRETS
        .lock()
        .map(|known| known.clone())
        .unwrap_or_default()
}

fn require_env_name(at: &str, kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('.') {
        return Err(format!("{at}: {kind}. must be followed by one name"));
    }
    Ok(())
}

fn function(at: &str, text: &str) -> Result<Segment, String> {
    let open = text.find('(').ok_or_else(|| unknown(at, text))?;
    let name = text[..open].trim();
    let inside = text[open + 1..]
        .strip_suffix(')')
        .ok_or_else(|| format!("{at}: {text:?} is missing its closing bracket"))?;
    let args = arguments(at, text, inside)?;

    let function = match (name, args.as_slice()) {
        ("rand", [Argument::Number(low), Argument::Number(high)]) => {
            if low > high {
                return Err(format!(
                    "{at}: rand({low}, {high}) is an empty range; the low value comes first"
                ));
            }
            Function::Rand {
                low: *low,
                high: *high,
            }
        }
        ("uuid", []) => Function::Uuid,
        ("seq", []) => Function::Seq,
        ("now", []) => Function::Now(Stamp::Iso),
        ("now", [Argument::Text(format)]) => match format.as_str() {
            "iso" | "iso8601" => Function::Now(Stamp::Iso),
            "unix" => Function::Now(Stamp::Unix),
            "unix_ms" => Function::Now(Stamp::UnixMs),
            other => {
                return Err(format!(
                    "{at}: now({other:?}) — the formats are 'iso8601', 'unix' and 'unix_ms'"
                ));
            }
        },
        ("pick", []) => return Err(format!("{at}: pick() has nothing to pick from")),
        ("pick", options) => Function::Pick(
            options
                .iter()
                .map(|argument| match argument {
                    Argument::Text(text) => Ok(text.clone()),
                    Argument::Number(number) => Ok(number.to_string()),
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
        ("rand" | "uuid" | "seq" | "now", _) => {
            return Err(format!(
                "{at}: {text:?} — rand takes two numbers, now takes an optional format, \
                 and uuid and seq take nothing"
            ));
        }
        _ => return Err(unknown(at, text)),
    };
    Ok(Segment::Call(function))
}

fn unknown(at: &str, text: &str) -> String {
    format!(
        "{at}: {text:?} is not one of the inline functions; they are rand, uuid, now, \
         seq and pick, and anything else is a generator's job"
    )
}

enum Argument {
    Text(String),
    Number(i64),
}

/// `'a', 'b'` or `1, 20`. Quoted with either quote, because a plan is JSON and a
/// double quote inside a JSON string has to be escaped to be written at all.
fn arguments(at: &str, whole: &str, inside: &str) -> Result<Vec<Argument>, String> {
    let inside = inside.trim();
    if inside.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = Vec::new();
    for piece in split(inside) {
        let piece = piece.trim();
        let quoted = (piece.starts_with('\'') && piece.ends_with('\''))
            || (piece.starts_with('"') && piece.ends_with('"'));
        if quoted && piece.len() >= 2 {
            args.push(Argument::Text(piece[1..piece.len() - 1].to_owned()));
        } else if let Ok(number) = piece.parse::<i64>() {
            args.push(Argument::Number(number));
        } else {
            return Err(format!(
                "{at}: {whole:?} — {piece:?} is neither a quoted string nor a whole number"
            ));
        }
    }
    Ok(args)
}

/// Split on commas that are not inside quotes, so `pick('a,b', 'c')` is two options.
fn split(inside: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (index, character) in inside.char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(open), other) if open == other => quote = None,
            (None, ',') => {
                pieces.push(&inside[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    pieces.push(&inside[start..]);
    pieces
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

    fn parse(text: &str) -> Result<Template, String> {
        Template::parse("at", text, &Datasets::default())
    }

    fn render(text: &str, pairs: &[(&str, &str)]) -> Result<String, Unbound> {
        let scope = scope(pairs);
        let datasets = Datasets::default();
        let mut values = Values::new(&scope, &datasets, 0, 0);
        parse(text).unwrap().render(&mut values)
    }

    #[test]
    fn a_template_with_no_variables_renders_itself() {
        let template = parse("/api/products").unwrap();
        assert!(template.is_fixed());
        assert_eq!(render("/api/products", &[]).unwrap(), "/api/products");
    }

    #[test]
    fn surrounding_text_is_kept_exactly() {
        assert_eq!(
            render("/api/orders/{{ id }}/items", &[("id", "A-1000")]).unwrap(),
            "/api/orders/A-1000/items"
        );
    }

    #[test]
    fn a_variable_with_nothing_behind_it_names_itself() {
        // Not an empty string: `/orders/` is a different request, and the service
        // answering it 404 would be reported as the service's problem.
        assert_eq!(
            render("/orders/{{ order_id }}", &[]),
            Err(Unbound("order_id".into()))
        );
    }

    #[test]
    fn a_captured_value_cannot_smuggle_a_second_query_parameter() {
        let scope = scope(&[("term", "a&b=c")]);
        let datasets = Datasets::default();
        let mut values = Values::new(&scope, &datasets, 0, 0);
        assert_eq!(
            parse("{{ term }}")
                .unwrap()
                .render_query(&mut values)
                .unwrap(),
            "a%26b%3Dc"
        );
    }

    #[test]
    fn an_unclosed_or_stray_brace_is_refused_at_compile_time() {
        assert!(parse("/a/{{ id").is_err());
        assert!(parse("/a/id }}").is_err());
        assert!(parse("/a/{{}}").is_err());
    }

    #[test]
    fn the_variables_a_template_reads_are_listed_in_order() {
        let template = parse("{{ a }}/x/{{ b }}").unwrap();
        assert_eq!(template.variables().collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn a_call_is_not_a_variable_an_earlier_step_has_to_have_captured() {
        let template = parse("/o/{{ uuid() }}").unwrap();
        assert_eq!(template.variables().count(), 0);
        assert!(!template.is_fixed());
    }

    #[test]
    fn rand_stays_inside_its_range_and_seq_counts_iterations() {
        let scope = Scope::new();
        let datasets = Datasets::default();
        for iteration in 0..20 {
            let mut values = Values::new(&scope, &datasets, 3, iteration);
            let rendered = parse("{{ rand(5,7) }}:{{ seq() }}")
                .unwrap()
                .render(&mut values)
                .unwrap();
            let (drawn, sequence) = rendered.split_once(':').unwrap();
            assert!(["5", "6", "7"].contains(&drawn), "{rendered}");
            assert_eq!(sequence, (iteration + 1).to_string());
        }
    }

    #[test]
    fn one_iteration_renders_the_same_request_every_time_it_is_replayed() {
        let scope = Scope::new();
        let datasets = Datasets::default();
        let once = |iteration| {
            let mut values = Values::new(&scope, &datasets, 42, iteration);
            parse("{{ uuid() }}/{{ pick('a','b','c') }}/{{ rand(1,1000) }}")
                .unwrap()
                .render(&mut values)
                .unwrap()
        };
        // A recorded seed is only worth recording if this holds.
        assert_eq!(once(7), once(7));
        assert_ne!(once(7), once(8));
    }

    #[test]
    fn two_calls_in_one_template_draw_different_values() {
        let scope = Scope::new();
        let datasets = Datasets::default();
        let mut values = Values::new(&scope, &datasets, 1, 1);
        let rendered = parse("{{ uuid() }} {{ uuid() }}")
            .unwrap()
            .render(&mut values)
            .unwrap();
        let (first, second) = rendered.split_once(' ').unwrap();
        // One stream, drawn from in order: a body needing two distinct ids gets two.
        assert_ne!(first, second);
    }

    #[test]
    fn pick_keeps_a_comma_that_is_inside_an_option() {
        let scope = Scope::new();
        let datasets = Datasets::default();
        let mut values = Values::new(&scope, &datasets, 1, 1);
        let rendered = parse("{{ pick('a,b') }}")
            .unwrap()
            .render(&mut values)
            .unwrap();
        assert_eq!(rendered, "a,b");
    }

    #[test]
    fn now_says_which_formats_it_has() {
        assert!(parse("{{ now() }}").is_ok());
        assert!(parse("{{ now('unix') }}").is_ok());
        assert!(parse("{{ now('unix_ms') }}").is_ok());
        let error = parse("{{ now('rfc822') }}").unwrap_err();
        assert!(error.contains("unix_ms"), "{error}");
    }

    #[test]
    fn a_misspelled_function_is_refused_rather_than_read_as_a_variable() {
        // Evaluating it as a variable called `uid()` and reporting it unbound would
        // send the plan's author looking for a step that was supposed to extract it.
        let error = parse("{{ uid() }}").unwrap_err();
        assert!(error.contains("generator"), "{error}");
        assert!(parse("{{ rand(1) }}").is_err());
        assert!(parse("{{ rand(9,1) }}").is_err());
        assert!(parse("{{ uuid(3) }}").is_err());
        assert!(parse("{{ pick() }}").is_err());
    }

    /// A variable every process running these tests has, so the test needs no
    /// `set_var` — which this workspace forbids, and which would be a race against
    /// every other test thread reading the environment anyway.
    const PRESENT: &str = if cfg!(windows) { "Path" } else { "PATH" };

    #[test]
    fn an_environment_value_is_read_once_and_becomes_part_of_the_text() {
        let template = parse(&format!("/r/{{{{ env.{PRESENT} }}}}")).unwrap();
        // A literal, not a variable: it does not change for the life of the run, so
        // it should not cost a lookup per request.
        assert!(template.is_fixed());
        let rendered = render(&format!("/r/{{{{ env.{PRESENT} }}}}"), &[]).unwrap();
        assert!(rendered.len() > "/r/".len(), "{rendered}");
    }

    #[test]
    fn a_credential_that_is_not_there_stops_the_plan_rather_than_every_request() {
        // Named by the variable the engine actually reads, so the operator knows what
        // to set. `secret.` has its own prefix: a bundle then says which credentials
        // it needs without carrying any of them.
        let error = parse("{{ secret.NOT_SET_ANYWHERE_9F3A }}").unwrap_err();
        assert!(
            error.contains("METRIX_SECRET_NOT_SET_ANYWHERE_9F3A"),
            "{error}"
        );
        let missing = parse("{{ env.NOT_SET_ANYWHERE_9F3A }}").unwrap_err();
        assert!(missing.contains("NOT_SET_ANYWHERE_9F3A"), "{missing}");
    }

    #[test]
    fn a_dotted_name_with_no_datasets_is_a_dataset_reference_and_says_so() {
        // Not a variable called "users.email": the dot means one thing, and telling
        // the author there is no such dataset is more use than telling them no step
        // extracted a variable nobody wrote.
        let error = parse("{{ users.email }}").unwrap_err();
        assert!(error.contains("no dataset named"), "{error}");
    }
}
