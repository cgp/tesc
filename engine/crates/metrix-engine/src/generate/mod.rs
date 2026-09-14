//! Building a request with something more than substitution.
//!
//! **A generator produces the whole request** (design-engine §7.1), not just the
//! body: path, query, headers and body. A hook that only returned a body could not
//! express "GET a random product id from the ids this VU has already seen", which is
//! most of what anyone reaches for a generator to do.
//!
//! Three tiers, all behind this one interface, because which tier a plan uses is a
//! cost decision rather than a semantic one:
//!
//! | tier | where | why |
//! |---|---|---|
//! | [`lua`] | one VM per worker thread, in process | the default for anything dynamic |
//! | [`plugin`] | compiled into the engine | when even Lua's per-call cost matters |
//! | [`exec`] | a pool of long-lived processes | a script that already exists |
//!
//! Two rules apply to every tier (§7.3):
//!
//! - **Generation time is measured and reported separately.** If generation is the
//!   slow part that has to be visible; folded into response latency it would make the
//!   service look slow instead.
//! - **Generation failure is its own error class**, never counted as a target error.
//!   The service was never asked.

pub(crate) mod corpus;
pub(crate) mod exec;
pub(crate) mod lua;
pub(crate) mod plugin;

use std::collections::BTreeMap;
use std::path::Path;

use metrix_plan::{Generator as Declared, Mix};

use crate::random::Rng;

/// What a generator may override, and what it left alone.
///
/// Every field optional, because a generator that had to return the whole request
/// would force a plan to move its headers out of the call document and into a script
/// to change one of them.
#[derive(Default)]
pub(crate) struct Built {
    pub path: Option<String>,
    pub query: Option<Vec<(String, String)>>,
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
}

/// What the hook is handed.
pub(crate) struct Context<'a> {
    /// The slot running this iteration: a virtual user's identity for the run.
    pub vu: usize,
    pub iteration: u64,
    pub step: &'a str,
    /// What earlier steps of this chain extracted.
    pub vars: &'a BTreeMap<String, String>,
    /// This iteration's row of each dataset, keyed by the dataset's name.
    pub rows: Vec<(&'a str, Vec<(&'a str, &'a str)>)>,
    /// The call's own arguments, already templated.
    pub args: &'a BTreeMap<String, String>,
    /// The iteration's stream, so a generator replays with the rest of the plan.
    pub rng: &'a mut Rng,
}

/// One declared generator, compiled.
pub(crate) enum Generator {
    Lua(lua::Script),
    Plugin(plugin::Registered),
    Exec(exec::Sidecar),
}

impl Generator {
    /// Build one request. Synchronous by construction for the in-process tiers, which
    /// is what lets a Lua VM live in a thread local rather than behind a lock.
    pub async fn build(&self, context: &mut Context<'_>) -> Result<Built, String> {
        match self {
            Self::Lua(script) => script.call(context),
            Self::Plugin(registered) => registered.call(context),
            Self::Exec(sidecar) => sidecar.call(context).await,
        }
    }
}

/// `prefetch` is declared in the format and is not available here.
///
/// A buffered request was built for the iteration that was current when it was built,
/// and it carries that iteration's dataset row, its `seq()` and its draws from the
/// seeded stream. Handing it to a later iteration would send one iteration's row
/// under another iteration's number, which quietly breaks the thing the whole seed
/// exists for: that a replay of the plan sends the same traffic. Doing it correctly
/// means the buffer is keyed by the iteration it was built for, which means
/// predicting admission through the scheduler — a scheduler change rather than a
/// generator one. Refused rather than approximated, because an approximation here is
/// a run whose numbers cannot be compared with another run's.
fn refuse_prefetch(at: &str, prefetch: Option<u32>) -> Result<(), String> {
    require(
        prefetch.is_none(),
        &format!(
            "{at}/prefetch: building requests ahead is not implemented. A buffered request              carries the dataset row and the seeded draws of the iteration it was built              for, so handing it to a later one would make the run unreplayable"
        ),
    )
}

/// Every generator the mix declares, in a fixed order so a call compiles to an index.
#[derive(Default)]
pub(crate) struct Generators {
    names: Vec<String>,
    generators: Vec<Generator>,
}

impl Generators {
    pub fn load(root: &Path, mix: &Mix) -> Result<Self, String> {
        let mut names = Vec::new();
        let mut generators = Vec::new();
        for (name, declared) in &mix.generators {
            let at = format!("mix.json/generators/{name}");
            require(!name.is_empty(), &format!("{at}: an empty generator name"))?;
            generators.push(match declared {
                Declared::Lua {
                    file,
                    entry,
                    corpus,
                    prefetch,
                } => {
                    refuse_prefetch(&at, *prefetch)?;
                    Generator::Lua(lua::Script::load(&at, root, file, entry, corpus.as_ref())?)
                }
                Declared::Plugin { name, prefetch } => {
                    refuse_prefetch(&at, *prefetch)?;
                    Generator::Plugin(plugin::Registered::find(&at, name)?)
                }
                Declared::Exec {
                    command,
                    protocol,
                    pool,
                    timeout_ms,
                    prefetch,
                } => {
                    refuse_prefetch(&at, *prefetch)?;
                    Generator::Exec(exec::Sidecar::prepare(
                        &at,
                        root,
                        command,
                        *protocol,
                        *pool,
                        *timeout_ms,
                    )?)
                }
            });
            names.push(name.clone());
        }
        Ok(Self { names, generators })
    }

    /// Resolve a call's `generator` name to the index it compiles to.
    pub fn resolve(&self, at: &str, name: &str) -> Result<usize, String> {
        self.names
            .iter()
            .position(|declared| declared == name)
            .ok_or_else(|| {
                format!(
                    "{at}: no generator named {name:?} is declared in mix.json{}",
                    if self.names.is_empty() {
                        String::new()
                    } else {
                        format!("; declared: {}", self.names.join(", "))
                    }
                )
            })
    }

    pub fn get(&self, index: usize) -> &Generator {
        &self.generators[index]
    }

    pub fn name(&self, index: usize) -> &str {
        &self.names[index]
    }

    /// The name, leaked, for the accumulator map it keys for the life of the run.
    pub fn leaked(&self, index: usize) -> &'static str {
        String::leak(self.names[index].clone())
    }

    /// Start whatever a tier needs running, once, before the arrival clock does.
    ///
    /// A sidecar that forked its first process on the first arrival would charge that
    /// fork to the first request's latency.
    pub async fn start(&self) -> Result<(), String> {
        for generator in &self.generators {
            if let Generator::Exec(sidecar) = generator {
                sidecar.start().await?;
            }
        }
        Ok(())
    }
}

/// A path inside the bundle, resolved the way datasets are.
///
/// The bundle is the unit that gets copied to a load box (§2.1), so a generator
/// reaching outside it is a plan that runs here and not there.
pub(crate) fn in_bundle(at: &str, root: &Path, file: &Path) -> Result<std::path::PathBuf, String> {
    let canonical = root
        .join(file)
        .canonicalize()
        .map_err(|_| format!("{at}/file: cannot open {file:?} inside the bundle"))?;
    require(
        canonical.starts_with(root),
        &format!("{at}/file: {file:?} is outside the bundle directory"),
    )?;
    Ok(canonical)
}

pub(crate) fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

/// Check what a generator handed back before it reaches the transport (§7.3).
///
/// A generator returning a malformed path fails the plan at first use rather than at
/// request ten thousand — and an unchecked one would reach `Uri::parse` inside the
/// send path, where the failure is a transport error against the service.
pub(crate) fn validate(name: &str, built: &Built) -> Result<(), String> {
    if let Some(path) = &built.path {
        require(
            path.starts_with('/') && !path.starts_with("//") && !path.contains('#'),
            &format!(
                "generator {name:?} returned {path:?}, which is not an origin-relative \
                 path without a fragment"
            ),
        )?;
        require(
            !path.contains(|c: char| c.is_ascii_control() || c == ' '),
            &format!("generator {name:?} returned a path holding a space or a control character"),
        )?;
    }
    for (key, value) in built.headers.iter().flatten() {
        require(
            hyper::header::HeaderName::from_bytes(key.as_bytes()).is_ok(),
            &format!("generator {name:?} returned {key:?}, which is not a header name"),
        )?;
        require(
            hyper::header::HeaderValue::from_str(value).is_ok(),
            &format!("generator {name:?} returned a value header {key} cannot hold"),
        )?;
        require(
            !crate::calls::transport_managed(key),
            &format!("generator {name:?} set {key}, which the transport owns"),
        )?;
    }
    Ok(())
}
