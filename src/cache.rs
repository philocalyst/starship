//! Support for rendering the prompt asynchronously.
//!
//! Each `starship prompt` is a fresh, short-lived process. To let the shell
//! draw a prompt immediately and update the slow parts (git status, language
//! versions, ...) a moment later, the init scripts split prompting into two
//! invocations (see [`ExecMode`]):
//!
//! * `starship prompt --cached` ([`ExecMode::CacheRead`]) — the fast paint the
//!   shell shows right away. Any module recorded by the previous refresh is
//!   served from the cache instead of recomputed, so the prompt is instant.
//!   Modules that were never recorded (the fast ones) are still computed live,
//!   so dynamic parts like the character or exit status stay correct.
//! * `starship prompt --deferred` ([`ExecMode::Refresh`]) — the background
//!   refresh. It renders *both* prompts (left and right), records the slow
//!   modules, and prints one "poke" line per repaint the shell should do (see
//!   [`crate::print::deferred`]): one when the refresh lands and — with
//!   `--watch` — one per configured `refresh_interval` tick, so dynamic
//!   modules keep advancing while the prompt sits idle. The cache is the only
//!   transport: the shell repaints by re-running the fast paint, never by
//!   capturing this process's output.
//! * plain `starship prompt` ([`ExecMode::Direct`]) — the classic synchronous
//!   render. Nothing touches the cache.
//!
//! Caching happens once, at the module dispatcher ([`crate::modules::handle`]),
//! and modules themselves are untouched. The refresh records each module that
//! took long enough to render (see `SLOW_MODULE_THRESHOLD` there), and the
//! fast paint replays exactly the recorded set — cache-entry presence is
//! itself the "this module is slow" signal, so fast, dynamic modules are
//! always computed live and never go stale.
//!
//! All of a directory's recorded modules live in a single snapshot file
//! written atomically once per refresh, so a concurrent fast paint sees either
//! the old or the new snapshot in full — never a torn mix of the two.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use sha1::{Digest, Sha1};

use crate::logger::get_log_dir;

/// How the current prompt render should treat slow work and the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecMode {
    /// Run everything directly (the classic, fully synchronous behavior).
    #[default]
    Direct,
    /// Fast paint: serve slow modules from the cache instead of computing them.
    CacheRead,
    /// Background refresh: compute everything and record the slow modules.
    Refresh,
}

static EXEC_MODE: OnceLock<ExecMode> = OnceLock::new();

/// Set the process-wide execution mode. A single starship invocation is always
/// either a fast paint, a background refresh, or a classic direct render, so
/// this is set once at startup and never changes.
pub fn set_exec_mode(mode: ExecMode) {
    let _ = EXEC_MODE.set(mode);
}

/// The current process-wide execution mode ([`ExecMode::Direct`] by default).
pub fn exec_mode() -> ExecMode {
    EXEC_MODE.get().copied().unwrap_or_default()
}

/// Cached snapshots only live for this long. This bounds how stale the fast
/// paint can be for a directory the refresh has stopped visiting.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn cache_dir() -> PathBuf {
    get_log_dir().join("cache")
}

/// Path of the snapshot holding every recorded module for `dir`, so a module
/// reuses its value only within the directory it was recorded for.
fn snapshot_path(dir: &Path) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(dir.as_os_str().as_encoded_bytes());
    let key: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    cache_dir().join(key)
}

/// Returns the age of a cache entry, or `None` if it cannot be determined.
fn entry_age(path: &Path) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
}

fn is_fresh(path: &Path) -> bool {
    entry_age(path).is_some_and(|age| age <= MAX_AGE)
}

fn is_stale(path: &Path) -> bool {
    entry_age(path).is_some_and(|age| age > MAX_AGE)
}

fn load_snapshot(path: &Path) -> HashMap<String, String> {
    if !is_fresh(path) {
        return HashMap::new();
    }
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// A slow module's rendered text from the last refresh of `dir`, if recorded.
/// The snapshot is read from disk once per process (a fast paint always
/// renders for a single directory).
pub fn lookup(module: &str, dir: &Path) -> Option<String> {
    static SNAPSHOT: OnceLock<HashMap<String, String>> = OnceLock::new();
    SNAPSHOT
        .get_or_init(|| load_snapshot(&snapshot_path(dir)))
        .get(module)
        .cloned()
}

/// Modules recorded so far during a refresh, buffered until [`flush`]. Modules
/// render in parallel (rayon), hence the mutex.
fn recorded() -> &'static Mutex<HashMap<String, String>> {
    static RECORDED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    RECORDED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a slow module's rendered text during a refresh.
pub fn record(module: &str, text: String) {
    if let Ok(mut map) = recorded().lock() {
        map.insert(module.to_string(), text);
    }
}

/// Atomically replace `dir`'s snapshot with everything recorded so far. A
/// module that stopped being slow simply isn't in the new snapshot anymore,
/// so it goes back to rendering live — no per-entry expiry needed.
pub fn flush(dir: &Path) {
    let Ok(map) = recorded().lock() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&*map) else {
        return;
    };
    if fs::create_dir_all(cache_dir()).is_err() {
        return;
    }

    // Write to a per-process temp file and rename so concurrent readers always
    // observe either the old or the new snapshot, never a partial write.
    let tmp = cache_dir().join(format!(".flush.{}.tmp", std::process::id()));
    if fs::write(&tmp, &bytes).is_ok() {
        let _ = fs::rename(&tmp, snapshot_path(dir));
    }
}

/// Delete snapshots older than [`MAX_AGE`]. Called by the refresh so abandoned
/// directories do not accumulate forever.
pub fn cleanup() {
    let Ok(entries) = fs::read_dir(cache_dir()) else {
        return;
    };

    for entry in entries.flatten() {
        if is_stale(&entry.path()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_path_distinguishes_directories() {
        assert_ne!(
            snapshot_path(Path::new("/repo")),
            snapshot_path(Path::new("/other")),
        );
        assert_eq!(
            snapshot_path(Path::new("/repo")),
            snapshot_path(Path::new("/repo")),
        );
    }

    #[test]
    fn flush_then_load_roundtrips_recorded_modules() {
        let dir = Path::new("/cache/test/roundtrip");
        record("git_status", "\u{1b}[31m[+1]\u{1b}[0m ".to_string());
        record("directory", "~/code ".to_string());
        flush(dir);

        let loaded = load_snapshot(&snapshot_path(dir));
        assert_eq!(
            loaded.get("git_status").map(String::as_str),
            Some("\u{1b}[31m[+1]\u{1b}[0m ")
        );
        assert_eq!(loaded.get("directory").map(String::as_str), Some("~/code "));

        let _ = fs::remove_file(snapshot_path(dir));
    }
}
