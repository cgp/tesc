//! Static files a generator reads: sample payloads, a fixture set, a word list.
//!
//! **Read once, before the run.** The whole point of letting a script read files is
//! convenience at setup, not I/O during the measured window (design-engine §7.2), so
//! everything under the declared directory is in memory before the arrival clock
//! starts. No streaming, no lazy reads, no file handles on the hot path.
//!
//! **A ceiling, checked at load.** A corpus is expected to be small enough to hold,
//! and "expected" is not a mechanism: a plan pointing at a directory that turns out
//! to hold a build tree is refused with both numbers, rather than quietly spending a
//! minute and a gigabyte before the first request.
//!
//! **Confined to the bundle**, resolved by canonicalising rather than by inspecting
//! the text, because the bundle is the unit that gets copied to a load box and a
//! symlink out of it is a plan that runs here and not there.

use std::path::Path;
use std::sync::Arc;

use metrix_plan::Corpus as Declared;

use super::require;

/// Enough for a fixture set and small enough that a directory nobody meant to point
/// at is refused instead of swallowed.
const DEFAULT_MAX_KB: u64 = 4096;

/// One generator's files, in memory, shared by every VM that runs its script.
#[derive(Default)]
pub(crate) struct Corpus {
    /// Sorted by name, so `corpus_names[3]` is the same file on every machine and a
    /// script that picks by index picks the same thing on a replay.
    files: Vec<(String, Arc<[u8]>)>,
}

impl Corpus {
    pub fn load(at: &str, root: &Path, declared: &Declared) -> Result<Self, String> {
        let at = format!("{at}/corpus");
        let dir = root
            .join(&declared.dir)
            .canonicalize()
            .map_err(|_| format!("{at}/dir: cannot open {:?} inside the bundle", declared.dir))?;
        require(
            dir.starts_with(root),
            &format!(
                "{at}/dir: {:?} is outside the bundle directory",
                declared.dir
            ),
        )?;
        require(
            dir.is_dir(),
            &format!("{at}/dir: {:?} is not a directory", declared.dir),
        )?;
        let ceiling = declared.max_kb.unwrap_or(DEFAULT_MAX_KB) * 1024;
        require(ceiling > 0, &format!("{at}/max_kb: must be positive"))?;

        let mut walk = Walk {
            at: &at,
            root,
            base: &dir,
            ceiling,
            files: Vec::new(),
            total: 0,
        };
        walk.read(&dir)?;
        let mut files = walk.files;
        require(
            !files.is_empty(),
            &format!("{at}/dir: {:?} holds no files", declared.dir),
        )?;
        // Sorted here rather than relying on the order the directory was walked in,
        // which is the filesystem's business and differs between machines.
        files.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Self { files })
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<[u8]>)> {
        self.files
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes))
    }

    /// Every name, for the message a script gets when it asks for one that is absent.
    pub fn names(&self) -> Vec<&str> {
        self.files.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// One walk of one corpus directory, carrying what the recursion needs.
struct Walk<'a> {
    at: &'a str,
    root: &'a Path,
    /// The corpus directory itself, which every key is relative to.
    base: &'a Path,
    ceiling: u64,
    files: Vec<(String, Arc<[u8]>)>,
    total: u64,
}

impl Walk<'_> {
    fn read(&mut self, dir: &Path) -> Result<(), String> {
        let at = self.at;
        let entries = std::fs::read_dir(dir)
            .map_err(|error| format!("{at}/dir: cannot read {} — {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{at}/dir: {error}"))?;
            let path = entry.path();
            // Canonicalised per entry: a symlink is a path inside the bundle to look
            // at and outside it to read.
            let real = path
                .canonicalize()
                .map_err(|_| format!("{at}/dir: cannot open {}", path.display()))?;
            require(
                real.starts_with(self.root),
                &format!(
                    "{at}/dir: {} leads outside the bundle directory",
                    relative(self.base, &path)
                ),
            )?;
            if real.is_dir() {
                self.read(&real)?;
                continue;
            }
            let bytes = std::fs::read(&real)
                .map_err(|error| format!("{at}/dir: cannot read {} — {error}", path.display()))?;
            self.total += bytes.len() as u64;
            require(
                self.total <= self.ceiling,
                &format!(
                    "{at}: the files under it total more than {} KB, which is the ceiling \
                     this corpus set. A corpus is held in memory for the whole run, so the \
                     ceiling is what stops a plan pointing at the wrong directory from \
                     taking the box with it",
                    self.ceiling / 1024
                ),
            )?;
            self.files.push((relative(self.base, &path), bytes.into()));
        }
        Ok(())
    }
}

/// A path under the corpus directory, with forward slashes on every platform.
///
/// A plan is written on one machine and run on another; a key that was
/// `payloads\order.xml` here and `payloads/order.xml` there would be a script that
/// works for its author and nobody else.
fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
