//! Where a completed module render is kept between prompts.
//!
//! Two decisions define this store, and both are corrections of the design it
//! replaces.
//!
//! **It holds structure, not presentation.** Entries are `Vec<Segment>` — styled
//! spans, before any decision about terminal width, palette resolution, or
//! shell-specific escaping. Those are applied when the prompt is painted, from
//! the live configuration, every time. Storing finished ANSI instead makes all
//! three part of the cache key by implication, and since they were not *in* the
//! key, a resized terminal or an edited palette silently replayed output that
//! no longer matched the request. Keeping segments means those inputs cannot go
//! stale, because they were never captured.
//!
//! **Validity is proved, not assumed.** An entry carries the [`Deps`] it was
//! computed under, so reuse is conditional on those observations still holding
//! ([`Entry::is_valid`]). There is no expiry: an entry is good until reality
//! moves, however long that takes, and worthless the moment it does. The
//! previous store had only a 24-hour ceiling and a directory-shaped key, which
//! is why a branch switch could be papered over for a paint.
//!
//! A snapshot is one file per directory, written atomically, so a concurrent
//! paint sees either the whole previous generation or the whole next one.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::segment::Segment;

use super::deps::{Deps, Envelope, WatchTarget};
use super::watchman::ChangeSet;

/// Bumped when the on-disk shape changes. An unreadable or differently-versioned
/// snapshot is discarded rather than migrated: the cost of being wrong is a
/// stale prompt, and the cost of discarding is one slow paint.
const FORMAT: u32 = 3;

/// On what terms a stored render may be shown again.
///
/// This is a field rather than an inference from the dependencies, because the
/// inference would be exactly wrong: a [`Profile::Sampled`] module records no
/// observations, and "no observations, none of which have changed" is
/// vacuously true. An empty [`Deps`] would therefore mark the *least* reusable
/// modules as permanently valid. Stating the terms explicitly makes that
/// unrepresentable.
///
/// [`Profile::Sampled`]: crate::prompt::keying::Profile::Sampled
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reuse {
    /// Reusable for as long as the recorded observations hold.
    WhileValid,
    /// Never reusable. May only be shown as the last known reading, and only
    /// by a caller that has said it will accept one.
    SampleOnly,
}

/// A module's completed render, plus everything needed to decide whether it may
/// be shown again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Ambient state at the time of computation — binary version and the
    /// module's own configuration subtree.
    envelope: Envelope,
    /// On what terms this may stand in for fresh work.
    reuse: Reuse,
    /// What was observed. Re-observing these is the whole validity check.
    deps: Deps,
    /// The render itself, still structured.
    segments: Vec<Segment>,
}

impl Entry {
    /// Whether this may stand in for a fresh computation right now.
    ///
    /// The envelope is checked first because it is a pure comparison against
    /// values already in memory, whereas re-observing dependencies touches the
    /// filesystem. An edited config therefore costs no syscalls to detect.
    pub fn is_valid(&self, envelope: &Envelope) -> bool {
        self.reuse == Reuse::WhileValid && self.envelope == *envelope && self.deps.still_holds()
    }

    /// As [`Self::is_valid`], except Watchman supplied a complete change set
    /// since this snapshot's checkpoint.
    fn is_valid_with_watcher(&self, envelope: &Envelope, changes: &ChangeSet) -> bool {
        self.reuse == Reuse::WhileValid
            && self.envelope == *envelope
            && self.deps.holds_with_changes(changes)
    }

    pub fn reuse(&self) -> Reuse {
        self.reuse
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn deps(&self) -> &Deps {
        &self.deps
    }
}

/// Every module recorded for one directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    format: u32,
    modules: HashMap<String, Entry>,
    /// A Watchman clock captured before the refresh rendered this snapshot.
    /// It is optional so Watchman is purely an optimization, never a cache
    /// dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watchman: Option<crate::prompt::watchman::Checkpoint>,
    /// Set while loading, never serialized. It contains Watchman's complete
    /// event history since `watchman`; `None` deliberately selects stat-only
    /// validation.
    #[serde(skip)]
    watchman_changes: Option<ChangeSet>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            format: FORMAT,
            modules: HashMap::new(),
            watchman: None,
            watchman_changes: None,
        }
    }
}

impl Snapshot {
    /// A valid entry for `module`, if one exists.
    ///
    /// Validity is checked here rather than by the caller so there is no way to
    /// obtain an entry without having proved it current — the type does not
    /// expose a "give me whatever you have" path, because every historical
    /// staleness bug in this area came from exactly that call.
    pub fn get(&self, module: &str, envelope: &Envelope) -> Option<&Entry> {
        self.modules.get(module).filter(|entry| {
            if let Some(changes) = &self.watchman_changes {
                entry.is_valid_with_watcher(envelope, changes)
            } else {
                entry.is_valid(envelope)
            }
        })
    }

    /// The entry for `module` whether or not it is still valid.
    ///
    /// Separate, and named to be conspicuous, because it has exactly one honest
    /// use: showing something rather than nothing while a refresh is in flight.
    /// Callers must mark the result as provisional.
    pub fn get_stale(&self, module: &str) -> Option<&Entry> {
        self.modules.get(module)
    }

    pub fn insert(&mut self, module: impl Into<String>, entry: Entry) {
        self.modules.insert(module.into(), entry);
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Whether this holds the same renders as `other`.
    ///
    /// A refresh that produced an identical snapshot has nothing to show, and
    /// asking a shell to repaint an unchanged prompt is a visible cost for no
    /// benefit -- it is what makes a polling design flicker. Comparing the
    /// stored structure is exact here precisely because entries are segments
    /// rather than pre-rendered text.
    pub fn matches(&self, other: &Self) -> bool {
        self.modules == other.modules
    }

    /// Everything an external file watcher may watch to know this snapshot has aged.
    ///
    /// The union across modules, deduplicated — language modules all observe
    /// the working directory, and that should produce one watch, not fifty.
    pub fn watch_targets(&self) -> Vec<WatchTarget> {
        let mut targets: Vec<WatchTarget> = Vec::new();
        for entry in self.modules.values() {
            for target in entry.deps.watch_targets() {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        targets
    }

    /// Modules whose observations no longer hold.
    ///
    /// This is what a refresh recomputes: not the whole prompt, only the parts
    /// reality has moved out from under. On a `cd` that is most of them; on a
    /// file save inside a repository it is usually just the git modules.
    pub fn invalid_modules(&self, envelope_for: impl Fn(&str) -> Envelope) -> Vec<String> {
        self.modules
            .iter()
            .filter(|(name, entry)| !entry.is_valid(&envelope_for(name)))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Attach a checkpoint taken before rendering. The caller must verify it
    /// has not advanced after the render before calling this method.
    pub fn set_watchman_checkpoint(
        &mut self,
        checkpoint: Option<crate::prompt::watchman::Checkpoint>,
    ) {
        self.watchman = checkpoint;
    }
}

/// Reads and writes snapshots, one file per directory.
///
/// A directory is the right granularity because it is the coarsest unit that
/// changes as a whole: a `cd` invalidates essentially every keyed module at
/// once, so splitting further would mean many reads for one prompt, and
/// combining further would mean rewriting unrelated directories on every save.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default location, alongside the logs.
    pub fn default_location() -> Self {
        Self::new(crate::logger::get_log_dir().join("prompt"))
    }

    fn path_for(&self, dir: &Path) -> PathBuf {
        let mut hasher = Sha1::new();
        hasher.update(dir.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        let name: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        self.root.join(name)
    }

    /// Load `dir`'s snapshot, or an empty one if it is missing, unreadable, or
    /// written by a different format version.
    pub fn load(&self, dir: &Path) -> Snapshot {
        let Ok(bytes) = fs::read(self.path_for(dir)) else {
            return Snapshot::default();
        };
        match serde_json::from_slice::<Snapshot>(&bytes) {
            Ok(mut snapshot) if snapshot.format == FORMAT => {
                snapshot.watchman_changes = snapshot.watchman.as_ref().and_then(|checkpoint| {
                    crate::prompt::watchman::changes(
                        checkpoint,
                        crate::prompt::watchman::Budget::Prompt,
                    )
                });
                snapshot
            }
            Ok(_) => Snapshot::default(),
            Err(error) => {
                log::debug!("Discarding unreadable prompt snapshot: {error}");
                Snapshot::default()
            }
        }
    }

    /// Replace `dir`'s snapshot atomically.
    ///
    /// Write-to-temp-then-rename, so a paint racing this write sees one
    /// generation or the other. A partially applied snapshot would be worse
    /// than a stale one — it would mix modules computed under different
    /// realities, which is precisely the torn read the previous per-module
    /// layout allowed.
    pub fn save(&self, dir: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let tmp = self.root.join(format!(".{}.tmp", std::process::id()));
        fs::write(&tmp, &bytes)?;
        // On failure the temp file would otherwise linger; the snapshot itself
        // is untouched, so the next paint simply uses the previous generation.
        fs::rename(&tmp, self.path_for(dir)).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// Build an entry that may be reused while its observations hold.
    pub fn keyed(envelope: Envelope, deps: Deps, segments: Vec<Segment>) -> Entry {
        Entry {
            envelope,
            reuse: Reuse::WhileValid,
            deps,
            segments,
        }
    }

    /// Build an entry that records a reading without claiming it stays true.
    pub fn sample(envelope: Envelope, segments: Vec<Segment>) -> Entry {
        Entry {
            envelope,
            reuse: Reuse::SampleOnly,
            deps: Deps::default(),
            segments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Style;

    fn segments(text: &str) -> Vec<Segment> {
        Segment::from_text(None, text)
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompt"));
        (dir, store)
    }

    #[test]
    fn entries_survive_a_round_trip_with_styles_intact() {
        let (_guard, store) = store();
        let dir = Path::new("/project");

        let styled = Segment::from_text(Some(Style::from(nu_ansi_term::Color::Red)), "main");
        let mut snapshot = Snapshot::default();
        snapshot.insert(
            "git_branch",
            Store::keyed(Envelope::new("v", "cfg"), Deps::default(), styled.clone()),
        );
        store.save(dir, &snapshot).unwrap();

        let loaded = store.load(dir);
        let entry = loaded.get_stale("git_branch").unwrap();
        assert_eq!(entry.segments().len(), styled.len());
        assert_eq!(
            entry.segments()[0].style(),
            styled[0].style(),
            "styles must round-trip structurally; flattening to ANSI is what \
             made palette and width changes unrepresentable",
        );
    }

    #[test]
    fn a_changed_config_invalidates_without_touching_the_filesystem() {
        let entry = Store::keyed(
            Envelope::new("v", "old-config"),
            Deps::default(),
            segments("x"),
        );

        assert!(entry.is_valid(&Envelope::new("v", "old-config")));
        assert!(
            !entry.is_valid(&Envelope::new("v", "new-config")),
            "editing a module's config must take effect on the very next paint, \
             not once a refresh happens to land",
        );
    }

    #[test]
    fn a_changed_binary_version_invalidates_everything() {
        let entry = Store::keyed(
            Envelope::new("1.0.0", "cfg"),
            Deps::default(),
            segments("x"),
        );
        assert!(!entry.is_valid(&Envelope::new("1.0.1", "cfg")));
    }

    #[test]
    fn get_refuses_to_hand_back_an_invalid_entry() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let deps = Deps::builder().path(file.path()).build();

        let mut snapshot = Snapshot::default();
        snapshot.insert(
            "nodejs",
            Store::keyed(Envelope::new("v", "cfg"), deps, segments("v20")),
        );

        let envelope = Envelope::new("v", "cfg");
        assert!(snapshot.get("nodejs", &envelope).is_some());

        fs::write(file.path(), "changed-length").unwrap();

        assert!(
            snapshot.get("nodejs", &envelope).is_none(),
            "the safe accessor must not return an entry whose observations broke",
        );
        assert!(
            snapshot.get_stale("nodejs").is_some(),
            "the deliberately-named accessor still offers it, for showing \
             something while a refresh is in flight",
        );
    }

    #[test]
    fn tree_entries_are_reused_only_after_a_clean_watchman_query() {
        let dir = tempfile::tempdir().unwrap();
        let deps = Deps::builder().tree(dir.path()).build();
        let envelope = Envelope::new("v", "cfg");
        let mut snapshot = Snapshot::default();
        snapshot.insert(
            "git_status",
            Store::keyed(envelope.clone(), deps, segments("clean")),
        );

        assert!(snapshot.get("git_status", &envelope).is_none());

        snapshot.watchman_changes = Some(ChangeSet::Clean);
        assert!(snapshot.get("git_status", &envelope).is_some());

        snapshot.watchman_changes = Some(ChangeSet::Paths(vec![dir.path().join("README.md")]));
        assert!(snapshot.get("git_status", &envelope).is_none());
    }

    #[test]
    fn a_foreign_format_is_discarded_rather_than_misread() {
        let (_guard, store) = store();
        let dir = Path::new("/project");
        fs::create_dir_all(&store.root).unwrap();
        fs::write(
            store.path_for(dir),
            serde_json::json!({ "format": 999, "modules": {} }).to_string(),
        )
        .unwrap();

        assert!(store.load(dir).is_empty());
    }

    #[test]
    fn unreadable_snapshots_degrade_to_an_empty_one() {
        let (_guard, store) = store();
        let dir = Path::new("/project");
        fs::create_dir_all(&store.root).unwrap();
        fs::write(store.path_for(dir), b"not json at all").unwrap();

        assert!(
            store.load(dir).is_empty(),
            "a corrupt cache costs a slow paint, never a wrong one"
        );
    }

    #[test]
    fn directories_do_not_share_snapshots() {
        let (_guard, store) = store();
        assert_ne!(
            store.path_for(Path::new("/a")),
            store.path_for(Path::new("/b")),
        );
    }
}
