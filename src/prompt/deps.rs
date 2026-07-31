//! What a rendered module depends on — the one declaration that drives
//! correctness, efficiency, and legibility.
//!
//! A [`Deps`] value is a *snapshot of observed reality*: the paths a module
//! looked at and what it saw there, the environment variables it read and what
//! they held, plus any opaque facts it wants to pin (a git HEAD id, say). It is
//! built by the module's [`Keying`] recipe, and then feeds three consumers that
//! would otherwise each need their own declaration:
//!
//! * **Correctness** — hashed into a [`Fingerprint`], it is the cache key.
//!   Reuse is valid exactly while the observations still hold, so there is no
//!   TTL to tune and no window in which a stale value is served.
//! * **Efficiency** — the optional Watchman bridge wakes a shell when the
//!   filesystem changes. The cache remains correct without it by re-observing
//!   these dependencies.
//! * **Legibility** — `starship explain` renders the observations directly, so
//!   "why is this module being recomputed?" has a literal answer.
//!
//! Keeping these three derived from a single source is what stops them from
//! drifting: a module cannot start reading a new file, and be watched for it,
//! but forget to key on it.
//!
//! [`Keying`]: super::keying::Keying

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::watchman::ChangeSet;

/// What was observed at a path. Absence is itself an observation: a module that
/// rendered because `package.json` was missing must be recomputed when one
/// appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stamp {
    /// Nothing exists at the path.
    Absent,
    /// A file, pinned by its modification time and length. The pair is far
    /// cheaper than hashing contents and catches ordinary edits. Watchman can
    /// additionally wake an idle shell after any filesystem event.
    File { mtime_ns: u128, len: u64 },
    /// A directory, pinned by its modification time — which changes when an
    /// entry is added or removed, exactly the event a detector cares about.
    Dir { mtime_ns: u128 },
}

impl Stamp {
    /// Observe `path` right now.
    fn observe(path: &Path) -> Self {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return Self::Absent;
        };
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());

        if meta.is_dir() {
            Self::Dir { mtime_ns }
        } else {
            Self::File {
                mtime_ns,
                len: meta.len(),
            }
        }
    }

    fn feed(&self, hasher: &mut Sha1) {
        match self {
            Self::Absent => hasher.update([0u8]),
            Self::File { mtime_ns, len } => {
                hasher.update([1u8]);
                hasher.update(mtime_ns.to_le_bytes());
                hasher.update(len.to_le_bytes());
            }
            Self::Dir { mtime_ns } => {
                hasher.update([2u8]);
                hasher.update(mtime_ns.to_le_bytes());
            }
        }
    }
}

/// How broadly a path observation describes filesystem state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PathScope {
    /// The path itself is the input. It can be re-statted without Watchman.
    #[default]
    Exact,
    /// Any change beneath this directory is an input. Only Watchman can prove
    /// that cheaply; without it the dependency is conservatively invalid.
    Tree,
}

/// A path that was looked at, what was there, and how broadly it matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathDep {
    pub path: PathBuf,
    pub stamp: Stamp,
    #[serde(default)]
    pub scope: PathScope,
}

/// An environment variable that was read, and what it held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDep {
    pub name: String,
    pub value: Option<String>,
}

/// An opaque fact a module pins itself, for state no path or variable captures
/// — a resolved git HEAD id, a parsed toolchain version, a config subtree hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub label: String,
    pub value: String,
}

/// A filesystem location relevant to a [`Deps`].
///
/// A path that was *present* is watched directly. A path that was *absent* is
/// watched via its parent directory, because there is nothing yet to watch and
/// the event that matters is its creation. Collapsing both into one type keeps
/// integrations from having to re-derive that distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTarget {
    /// Watch this exact path for modification or removal.
    Exact(PathBuf),
    /// Watch this directory for an entry appearing.
    Parent(PathBuf),
    /// Watch a complete directory tree.
    Tree(PathBuf),
}

impl WatchTarget {
    /// The path a filesystem watcher should actually be registered on.
    pub fn path(&self) -> &Path {
        match self {
            Self::Exact(p) | Self::Parent(p) | Self::Tree(p) => p,
        }
    }
}

/// The observations a module's output rests on.
///
/// Build one with [`Deps::builder`]; the builder observes as it records, so a
/// finished `Deps` is a coherent snapshot rather than a list of intentions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deps {
    paths: Vec<PathDep>,
    env: Vec<EnvDep>,
    marks: Vec<Mark>,
}

impl Deps {
    pub fn builder() -> DepsBuilder {
        DepsBuilder(Self::default())
    }

    pub fn paths(&self) -> &[PathDep] {
        &self.paths
    }

    pub fn env(&self) -> &[EnvDep] {
        &self.env
    }

    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// Whether this records no observations at all. An empty `Deps` would key
    /// every directory and every state to the same value, so callers treat it
    /// as "not cacheable" rather than "cacheable under a trivial key" — a
    /// module with nothing to key on is volatile, not universally reusable.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.env.is_empty() && self.marks.is_empty()
    }

    /// The filesystem watch set implied by these observations.
    ///
    /// Deduplicated, because unrelated modules routinely observe the same paths
    /// (every language module stats the working directory).
    pub fn watch_targets(&self) -> Vec<WatchTarget> {
        let mut targets: Vec<WatchTarget> = Vec::with_capacity(self.paths.len());
        for dep in &self.paths {
            let target = match dep.scope {
                PathScope::Tree => WatchTarget::Tree(dep.path.clone()),
                PathScope::Exact => match dep.stamp {
                    Stamp::Absent => match dep.path.parent() {
                        Some(parent) => WatchTarget::Parent(parent.to_path_buf()),
                        // A root path that does not exist: nothing sane to watch.
                        None => continue,
                    },
                    _ => WatchTarget::Exact(dep.path.clone()),
                },
            };
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    /// Re-observe every recorded path and variable, and report whether reality
    /// still matches. This is the fallback staleness check when Watchman is
    /// unavailable.
    ///
    /// Marks are deliberately *not* re-checked — they are facts the module had
    /// to do real work to produce (that being why they were pinned rather than
    /// observed), so verifying them would cost as much as recomputing. Marks
    /// therefore pin a module to the state it was computed under; the paths and
    /// variables alongside them are what detect that the state moved on.
    pub fn still_holds(&self) -> bool {
        self.paths
            .iter()
            .all(|dep| dep.scope == PathScope::Exact && Stamp::observe(&dep.path) == dep.stamp)
            && self.env_still_holds()
    }

    /// Validate against Watchman's event history. Exact inputs are still
    /// re-statted because the Watchman project need not cover tool binaries or
    /// other paths outside the repository. Tree inputs are valid precisely
    /// when the event stream says their subtree was untouched.
    pub fn holds_with_changes(&self, changes: &ChangeSet) -> bool {
        self.paths.iter().all(|dep| match dep.scope {
            PathScope::Exact => Stamp::observe(&dep.path) == dep.stamp,
            PathScope::Tree => !changes.touches_tree(&dep.path),
        }) && self.env_still_holds()
    }

    /// Re-observe only process-local inputs. When Watchman has proved the
    /// filesystem unchanged, these are the remaining mutable dependencies.
    pub fn env_still_holds(&self) -> bool {
        self.env.iter().all(|dep| read_env(&dep.name) == dep.value)
    }

    /// Hash these observations together with `envelope` into a cache key.
    ///
    /// Field kinds are tagged and lengths are prefixed so that observations
    /// cannot be reinterpreted across boundaries — without that, a path named
    /// for a variable's value could collide with the variable itself.
    pub fn fingerprint(&self, envelope: &Envelope) -> Fingerprint {
        let mut hasher = Sha1::new();
        envelope.feed(&mut hasher);

        hasher.update(b"paths");
        hasher.update((self.paths.len() as u64).to_le_bytes());
        for dep in &self.paths {
            feed_bytes(&mut hasher, dep.path.as_os_str().as_encoded_bytes());
            hasher.update([match dep.scope {
                PathScope::Exact => 0,
                PathScope::Tree => 1,
            }]);
            dep.stamp.feed(&mut hasher);
        }

        hasher.update(b"env");
        hasher.update((self.env.len() as u64).to_le_bytes());
        for dep in &self.env {
            feed_bytes(&mut hasher, dep.name.as_bytes());
            match &dep.value {
                Some(value) => {
                    hasher.update([1u8]);
                    feed_bytes(&mut hasher, value.as_bytes());
                }
                None => hasher.update([0u8]),
            }
        }

        hasher.update(b"marks");
        hasher.update((self.marks.len() as u64).to_le_bytes());
        for mark in &self.marks {
            feed_bytes(&mut hasher, mark.label.as_bytes());
            feed_bytes(&mut hasher, mark.value.as_bytes());
        }

        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Fingerprint(bytes)
    }
}

/// Length-prefixed so adjacent fields cannot be confused for one another.
fn feed_bytes(hasher: &mut Sha1, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn read_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Records observations as it makes them.
#[derive(Debug)]
pub struct DepsBuilder(Deps);

impl DepsBuilder {
    /// Observe a path. Recorded whether or not it exists — see [`Stamp`].
    #[must_use]
    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let stamp = Stamp::observe(&path);
        self.0.paths.push(PathDep {
            path,
            stamp,
            scope: PathScope::Exact,
        });
        self
    }

    /// Record a complete source tree. This is intentionally not reusable by
    /// stat-only validation: directory mtimes do not move when an existing
    /// nested file changes. It becomes reusable when Watchman proves that no
    /// event touched the tree.
    #[must_use]
    pub fn tree(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let stamp = Stamp::observe(&path);
        self.0.paths.push(PathDep {
            path,
            stamp,
            scope: PathScope::Tree,
        });
        self
    }

    /// Observe several paths under a common root, as a detector would.
    #[must_use]
    pub fn paths_in<I, S>(self, root: &Path, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        names
            .into_iter()
            .fold(self, |builder, name| builder.path(root.join(name.as_ref())))
    }

    /// Observe an environment variable.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        let value = read_env(&name);
        self.0.env.push(EnvDep { name, value });
        self
    }

    /// Pin an opaque fact the module produced itself.
    #[must_use]
    pub fn mark(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.marks.push(Mark {
            label: label.into(),
            value: value.into(),
        });
        self
    }

    /// Pin a fact only when it is known, so callers can thread through an
    /// `Option` without breaking the builder chain.
    #[must_use]
    pub fn maybe_mark(self, label: impl Into<String>, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.mark(label, value),
            None => self,
        }
    }

    pub fn build(self) -> Deps {
        self.0
    }
}

/// Ambient state that invalidates every module at once, mixed into every
/// [`Fingerprint`] so no module has to remember it.
///
/// This is deliberately small, and it is small *because* the store holds
/// structured segments rather than rendered ANSI. Terminal width, the active
/// palette, and the shell's escaping rules are all applied when the prompt is
/// painted, so none of them can invalidate a cached module and none of them
/// belong here. Caching presentation is what would force them in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The binary that produced the entry. Module output is not stable across
    /// versions, and this also makes the hash construction itself free to
    /// change: a differently-built key cannot be misread as an old one.
    version: String,
    /// A hash of the module's own configuration subtree. Scoping this per
    /// module rather than per file means editing the `[git_status]` table does
    /// not invalidate every language module in the prompt.
    config: String,
}

impl Envelope {
    pub fn new(version: impl Into<String>, config: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            config: config.into(),
        }
    }

    /// Hash a module's configuration subtree into the stable form used above.
    /// `None` (no configuration at all) is distinct from an empty table.
    pub fn hash_config(config: Option<&toml::Value>) -> String {
        let mut hasher = Sha1::new();
        match config {
            Some(value) => {
                hasher.update([1u8]);
                // `toml` preserves order (see Cargo.toml), so serializing is
                // deterministic for a given file.
                hasher.update(value.to_string().as_bytes());
            }
            None => hasher.update([0u8]),
        }
        hex(&hasher.finalize())
    }

    fn feed(&self, hasher: &mut Sha1) {
        feed_bytes(hasher, self.version.as_bytes());
        feed_bytes(hasher, self.config.as_bytes());
    }
}

/// A cache key: the digest of a module's observations plus its [`Envelope`].
///
/// Equality is the whole contract. Two renders with equal fingerprints were
/// produced under indistinguishable conditions, so one may stand in for the
/// other; unequal fingerprints say nothing about how *much* changed, only that
/// something did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint([u8; 16]);

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> Envelope {
        Envelope::new("test", "cfg")
    }

    #[test]
    fn absent_and_present_paths_key_differently() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.json");

        let absent = Deps::builder().path(&file).build();
        fs::write(&file, "{}").unwrap();
        let present = Deps::builder().path(&file).build();

        assert_ne!(
            absent.fingerprint(&envelope()),
            present.fingerprint(&envelope()),
            "a module that rendered because a file was missing must not reuse \
             that render once the file exists"
        );
    }

    #[test]
    fn still_holds_tracks_reality() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v");
        fs::write(&file, "1").unwrap();

        let deps = Deps::builder().path(&file).build();
        assert!(deps.still_holds());

        // Length differs, so this is caught regardless of mtime granularity.
        fs::write(&file, "22").unwrap();
        assert!(!deps.still_holds());
    }

    #[test]
    fn absent_paths_are_watched_via_their_parent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("Cargo.toml");

        let deps = Deps::builder().path(&missing).build();

        assert_eq!(
            deps.watch_targets(),
            vec![WatchTarget::Parent(dir.path().to_path_buf())],
            "there is nothing to watch at a path that does not exist; the event \
             that matters is its creation, which only the parent sees"
        );
    }

    #[test]
    fn watch_targets_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, "x").unwrap();

        let deps = Deps::builder().path(&file).path(&file).build();

        assert_eq!(deps.watch_targets().len(), 1);
    }

    #[test]
    fn tree_dependencies_require_watchman_for_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let deps = Deps::builder().tree(dir.path()).build();

        assert!(
            !deps.still_holds(),
            "a directory stat cannot prove its descendants are unchanged"
        );
        assert!(deps.holds_with_changes(&ChangeSet::Clean));
        assert!(!deps.holds_with_changes(&ChangeSet::Paths(vec![dir.path().join("src/lib.rs")])));
        assert_eq!(
            deps.watch_targets(),
            vec![WatchTarget::Tree(dir.path().to_path_buf())]
        );
    }

    #[test]
    fn envelope_scopes_config_per_module() {
        let git: toml::Value = toml::from_str("disabled = false").unwrap();
        let node: toml::Value = toml::from_str("format = \"$version\"").unwrap();

        assert_ne!(
            Envelope::hash_config(Some(&git)),
            Envelope::hash_config(Some(&node)),
        );
        assert_ne!(
            Envelope::hash_config(None),
            Envelope::hash_config(Some(&toml::Value::Table(toml::Table::new()))),
            "no configuration and an empty table are different states"
        );
    }

    #[test]
    fn fields_cannot_be_reinterpreted_across_kinds() {
        // Without length prefixes and kind tags, an observation recorded as one
        // kind could hash identically to a different one recorded as another.
        let as_mark = Deps::builder().mark("a", "bc").build();
        let as_marks = Deps::builder().mark("ab", "c").build();

        assert_ne!(
            as_mark.fingerprint(&envelope()),
            as_marks.fingerprint(&envelope()),
        );
    }

    #[test]
    fn envelope_version_invalidates_everything() {
        let deps = Deps::builder().mark("k", "v").build();

        assert_ne!(
            deps.fingerprint(&Envelope::new("1.0.0", "cfg")),
            deps.fingerprint(&Envelope::new("1.0.1", "cfg")),
        );
    }
}
