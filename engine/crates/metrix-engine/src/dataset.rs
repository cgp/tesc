//! CSV and JSONL rows, read once at load and shared by every iteration.
//!
//! The cheapest generation tier there is (design-engine §7.2): a row is picked with a
//! modulo and a field is read by index, so a plan that varies its traffic from a file
//! pays nothing per request that a plan sending one fixed body does not.
//!
//! **Read entirely into memory before the run starts.** No file handles on the hot
//! path and no I/O inside the measured window — a generator that read from disk per
//! request would put the test machine's page cache into the latency distribution.
//!
//! **Which row an iteration gets is a pure function of the iteration number.** No
//! cursor, no atomic, nothing shared between workers. That is what makes a replay of
//! the plan send the same rows in the same order, and it is why `round_robin` really
//! is round robin rather than "round robin per worker".

use std::collections::BTreeMap;
use std::path::Path;

use metrix_plan::{DatasetMode, Mix};

use crate::random::mix;

/// One file, loaded.
pub(crate) struct Dataset {
    name: String,
    columns: Vec<String>,
    /// Row-major, `width` values per row. One allocation rather than one per row: a
    /// dataset is read once and then only indexed.
    values: Vec<String>,
    width: usize,
    mode: DatasetMode,
    /// Distinguishes this dataset's stream from the others, so two `random` datasets
    /// in one plan do not march in lockstep and send row 12 of each together.
    salt: u64,
}

impl Dataset {
    pub fn rows(&self) -> usize {
        // A file with no columns has no rows, and never reaches here: an empty
        // header is refused at load.
        self.values.len().checked_div(self.width).unwrap_or(0)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mode(&self) -> DatasetMode {
        self.mode
    }

    /// The row this iteration reads.
    pub fn row(&self, iteration: u64, seed: u64) -> usize {
        let rows = self.rows() as u64;
        match self.mode {
            DatasetMode::RoundRobin => (iteration % rows) as usize,
            DatasetMode::Random => (mix(seed ^ iteration ^ self.salt) % rows) as usize,
            // The iteration number itself. Checked against the file's length at load,
            // so this cannot wrap and quietly start colliding.
            DatasetMode::UniquePerIteration => (iteration % rows) as usize,
        }
    }

    pub fn field(&self, row: usize, column: usize) -> &str {
        &self.values[row * self.width + column]
    }

    /// One whole row, named, for a generator that is handed the row rather than a
    /// field of it.
    pub fn fields(&self, row: usize) -> impl Iterator<Item = (&str, &str)> {
        self.columns
            .iter()
            .enumerate()
            .map(move |(column, name)| (name.as_str(), self.field(row, column)))
    }
}

/// Every dataset the mix declares, in a fixed order so references compile to indices.
#[derive(Default)]
pub(crate) struct Datasets {
    sets: Vec<Dataset>,
}

impl Datasets {
    /// Read every declared file, relative to the bundle directory.
    pub fn load(root: &Path, mix: &Mix) -> Result<Self, String> {
        let mut sets = Vec::new();
        for (name, spec) in &mix.datasets {
            let at = format!("mix.json#/datasets/{name}");
            require(
                !name.is_empty() && !name.contains('.'),
                &format!(
                    "{at}: a dataset name may not contain a dot; the dot is what \
                     separates the dataset from the field in {{{{ {name}.field }}}}"
                ),
            )?;
            let path = resolve(root, &spec.file, &at)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("{at}/file: cannot read {:?} — {error}", spec.file))?;
            let (columns, values) = match extension(&spec.file) {
                Some("csv") => csv(&at, &text)?,
                Some("jsonl") | Some("ndjson") => jsonl(&at, &text)?,
                _ => {
                    return Err(format!(
                        "{at}/file: expected a .csv or .jsonl file, got {:?}",
                        spec.file
                    ));
                }
            };
            require(
                !values.is_empty(),
                &format!("{at}/file: {:?} has a header and no rows", spec.file),
            )?;
            sets.push(Dataset {
                salt: mix_name(name),
                width: columns.len(),
                name: name.clone(),
                columns,
                values,
                mode: spec.mode,
            });
        }
        Ok(Self { sets })
    }

    /// Resolve `users.email` to the indices a template renders from.
    ///
    /// A name with no dot is a chain variable and is not this module's business; a
    /// name with one is a dataset reference, right or wrong. Both halves are checked
    /// here because a misspelled column is a load-time error in the plan, and finding
    /// it four minutes into a run would be finding it in the results.
    pub fn resolve(&self, at: &str, reference: &str) -> Result<Option<(usize, usize)>, String> {
        let Some((set, field)) = reference.split_once('.') else {
            return Ok(None);
        };
        require(
            !field.contains('.'),
            &format!("{at}: {reference:?} has more than one dot; datasets are flat rows"),
        )?;
        let index = self
            .sets
            .iter()
            .position(|dataset| dataset.name == set)
            .ok_or_else(|| {
                format!(
                    "{at}: no dataset named {set:?} is declared{}",
                    listing(self.sets.iter().map(|dataset| dataset.name.as_str()))
                )
            })?;
        let dataset = &self.sets[index];
        let column = dataset
            .columns
            .iter()
            .position(|column| column == field)
            .ok_or_else(|| {
                format!(
                    "{at}: dataset {set:?} has no field named {field:?}{}",
                    listing(dataset.columns.iter().map(String::as_str))
                )
            })?;
        Ok(Some((index, column)))
    }

    pub fn get(&self, index: usize) -> &Dataset {
        &self.sets[index]
    }

    /// A dataset by name, for the places that name one whole rather than a field of
    /// it — `identity: from_dataset:users`.
    pub fn index(&self, at: &str, name: &str) -> Result<usize, String> {
        self.sets
            .iter()
            .position(|dataset| dataset.name == name)
            .ok_or_else(|| {
                format!(
                    "{at}: no dataset named {name:?} is declared{}",
                    listing(self.sets.iter().map(|dataset| dataset.name.as_str()))
                )
            })
    }

    pub fn iter(&self) -> impl Iterator<Item = &Dataset> {
        self.sets.iter()
    }
}

/// `; declared: a, b` — the names that were available, for a misspelling.
fn listing<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("; declared: {}", names.join(", "))
    }
}

/// A dataset path, confined to the bundle.
///
/// The bundle is the unit that gets copied to a load box (design-engine §4), so a
/// plan reaching outside it is a plan that runs here and not there. Checked by
/// canonicalising rather than by inspecting the text, because `data/../../etc` is a
/// path inside the bundle to look at and outside it to read.
fn resolve(root: &Path, file: &Path, at: &str) -> Result<std::path::PathBuf, String> {
    let joined = root.join(file);
    let canonical = joined
        .canonicalize()
        .map_err(|_| format!("{at}/file: cannot open {file:?} inside the bundle"))?;
    require(
        canonical.starts_with(root),
        &format!("{at}/file: {file:?} is outside the bundle directory"),
    )?;
    Ok(canonical)
}

fn extension(file: &Path) -> Option<&str> {
    file.extension()?.to_str().map(str::trim)
}

fn mix_name(name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in name.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix(hash)
}

/// RFC 4180 with the parts a data file actually uses: a header row, quoted fields,
/// doubled quotes inside them, and CRLF or LF line endings.
fn csv(at: &str, text: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut line = 1;
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' if field.is_empty() => quoted = true,
            '"' => {
                return Err(format!(
                    "{at}/file: line {line}: a quote inside a bare field"
                ));
            }
            ',' if !quoted => record.push(std::mem::take(&mut field)),
            '\r' if !quoted && chars.peek() == Some(&'\n') => {}
            '\n' if !quoted => {
                line += 1;
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            other => {
                if other == '\n' {
                    line += 1;
                }
                field.push(other);
            }
        }
    }
    if quoted {
        return Err(format!("{at}/file: a quoted field is never closed"));
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    // A trailing newline is not an empty row.
    records.retain(|record| record.len() > 1 || record.first().is_some_and(|f| !f.is_empty()));

    let mut records = records.into_iter();
    let columns = records
        .next()
        .ok_or_else(|| format!("{at}/file: the file is empty; a CSV dataset needs a header row"))?;
    check_columns(at, &columns)?;

    let mut values = Vec::with_capacity(records.len() * columns.len());
    for (index, record) in records.enumerate() {
        require(
            record.len() == columns.len(),
            &format!(
                "{at}/file: row {} has {} fields and the header has {}",
                index + 1,
                record.len(),
                columns.len()
            ),
        )?;
        values.extend(record);
    }
    Ok((columns, values))
}

/// One JSON object per line. The first line's keys are the columns, and every later
/// line must carry all of them — a row missing a field would render a request with a
/// hole in it, and the plan says nothing about what belongs there.
fn jsonl(at: &str, text: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let mut columns: Vec<String> = Vec::new();
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let object: BTreeMap<String, serde_json::Value> = serde_json::from_str(line)
            .map_err(|error| format!("{at}/file: line {}: {error}", index + 1))?;
        if columns.is_empty() {
            columns = object.keys().cloned().collect();
            check_columns(at, &columns)?;
        }
        for column in &columns {
            let value = object
                .get(column)
                .ok_or_else(|| format!("{at}/file: line {}: no field {column:?}", index + 1))?;
            values.push(scalar(at, index + 1, column, value)?);
        }
    }
    require(
        !columns.is_empty(),
        &format!("{at}/file: the file holds no JSON objects"),
    )?;
    Ok((columns, values))
}

/// A field as the text a template substitutes.
///
/// Objects and arrays are refused rather than serialised: `{{ users.address }}`
/// putting `{"city":"Perth"}` into a URL is not what anybody meant, and a dataset is
/// a table.
fn scalar(
    at: &str,
    line: usize,
    column: &str,
    value: &serde_json::Value,
) -> Result<String, String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Bool(flag) => Ok(flag.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err(format!(
            "{at}/file: line {line}: field {column:?} is a {} and a dataset holds flat values",
            if value.is_array() { "list" } else { "object" }
        )),
    }
}

fn check_columns(at: &str, columns: &[String]) -> Result<(), String> {
    for (index, column) in columns.iter().enumerate() {
        require(
            !column.trim().is_empty(),
            &format!("{at}/file: column {index} has no name"),
        )?;
        require(
            !column.contains('.'),
            &format!(
                "{at}/file: column {column:?} contains a dot, which separates the \
                 dataset from the field in a reference"
            ),
        )?;
        require(
            !columns[..index].contains(column),
            &format!("{at}/file: two columns are named {column:?}"),
        )?;
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(at: &str, text: &str) -> (Vec<String>, Vec<String>) {
        csv(at, text).unwrap()
    }

    #[test]
    fn a_header_names_the_columns_and_the_rest_are_rows() {
        let (columns, values) = load("at", "email,region\na@x,apac\nb@x,emea\n");
        assert_eq!(columns, ["email", "region"]);
        assert_eq!(values, ["a@x", "apac", "b@x", "emea"]);
    }

    #[test]
    fn a_quoted_field_keeps_its_commas_quotes_and_newlines() {
        let (_, values) = load("at", "a,b\n\"x,y\",\"say \"\"hi\"\"\"\n");
        assert_eq!(values, ["x,y", "say \"hi\""]);
        let (_, multiline) = load("at", "a\n\"one\ntwo\"\n");
        assert_eq!(multiline, ["one\ntwo"]);
    }

    #[test]
    fn windows_line_endings_are_not_part_of_the_last_field() {
        let (columns, values) = load("at", "a,b\r\n1,2\r\n");
        assert_eq!(columns, ["a", "b"]);
        assert_eq!(values, ["1", "2"]);
    }

    #[test]
    fn a_short_row_says_which_one_it_is() {
        let error = csv("at", "a,b\n1,2\n3\n").unwrap_err();
        // Naming the row is the whole value of the check: a file with one bad line
        // in ten thousand is the case this exists for.
        assert!(error.contains("row 2"), "{error}");
    }

    #[test]
    fn a_duplicate_column_is_refused() {
        let error = csv("at", "a,a\n1,2\n").unwrap_err();
        assert!(error.contains("two columns"), "{error}");
    }

    #[test]
    fn jsonl_takes_its_columns_from_the_first_line() {
        let (columns, values) = jsonl(
            "at",
            "{\"id\": 1, \"name\": \"a\"}\n{\"id\": 2, \"name\": \"b\"}\n",
        )
        .unwrap();
        // Numbers become their text: a template substitutes text.
        assert_eq!(columns, ["id", "name"]);
        assert_eq!(values, ["1", "a", "2", "b"]);
    }

    #[test]
    fn a_jsonl_row_missing_a_field_is_refused() {
        let error = jsonl("at", "{\"id\": 1, \"name\": \"a\"}\n{\"id\": 2}\n").unwrap_err();
        assert!(
            error.contains("line 2") && error.contains("name"),
            "{error}"
        );
    }

    #[test]
    fn a_nested_value_is_refused_rather_than_serialised() {
        let error = jsonl("at", "{\"a\": {\"city\": \"Perth\"}}\n").unwrap_err();
        assert!(error.contains("flat values"), "{error}");
    }

    fn dataset(mode: DatasetMode, rows: usize) -> Dataset {
        Dataset {
            name: "users".into(),
            columns: vec!["id".into()],
            values: (0..rows).map(|row| row.to_string()).collect(),
            width: 1,
            mode,
            salt: mix_name("users"),
        }
    }

    #[test]
    fn round_robin_visits_every_row_in_order_and_wraps() {
        let set = dataset(DatasetMode::RoundRobin, 3);
        let seen: Vec<usize> = (0..7).map(|i| set.row(i, 0)).collect();
        assert_eq!(seen, [0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn random_is_the_same_on_a_replay_and_differs_between_datasets() {
        let one = dataset(DatasetMode::Random, 100);
        let mut two = dataset(DatasetMode::Random, 100);
        two.name = "orders".into();
        two.salt = mix_name("orders");

        let rows = |set: &Dataset| (0..50).map(|i| set.row(i, 9)).collect::<Vec<_>>();
        assert_eq!(rows(&one), rows(&one));
        // Two random datasets must not send row 12 of each together: a plan reading a
        // user and a product would then only ever pair the twelfth of each.
        assert_ne!(rows(&one), rows(&two));
    }
}
