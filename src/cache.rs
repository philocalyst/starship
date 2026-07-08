//! Support for rendering the prompt asynchronously.
//!
//! Each `starship prompt` is a fresh, short-lived process. To let the shell
//! draw a prompt immediately and refresh the slow parts (git status, language
//! versions, ...) a moment later, a prompt render happens in one of three modes
//! (see [`ExecMode`]):
//!
//! * [`ExecMode::CacheRead`] — the fast paint the shell shows right away. Any
//! module whose output was recorded by the previous refresh is served from
//! the cache instead of recomputed, so the prompt is instant. Modules that
//! were never recorded (the fast ones) are still computed live, so dynamic
//! parts like the character or exit status stay correct.
//! * [`ExecMode::Refresh`] — the background pass the shell runs just after. It
//! computes every module and records the *slow* ones in the cache; the shell
//! captures its output and redraws the prompt with it.
//! * [`ExecMode::Direct`] — the classic synchronous render, used when async
//! prompting is off. Nothing touches the cache.
//!
//! Caching happens once, at the module dispatcher ([`crate::modules::handle`]),
//! by storing each slow module's rendered text keyed by module name and
//! directory. Modules themselves are untouched, and whether a module is "slow"
//! is decided automatically from how long it took to compute. When the cache is
//! disabled (`STARSHIP_ASYNC_CACHE=0`) or empty, the fast paint simply has
//! nothing to show for those modules until the refresh lands.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::{Serialize, de::DeserializeOwned};
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

/// Whether the on-disk cache is in use. Enabled by default while async
/// prompting is active; set `STARSHIP_ASYNC_CACHE=0` to render slow modules
/// blank until each refresh instead of showing their previous values.
pub fn cache_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("STARSHIP_ASYNC_CACHE").map_or(true, |v| v != "0" && !v.is_empty())
    })
}

/// Cached values only live for this long. This bounds how stale the fast paint
/// can be for a directory the refresh has stopped visiting.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn cache_dir() -> PathBuf {
    get_log_dir().join("cache")
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

/// Cache key for a module's rendered output in a given directory, so the same
/// module reuses its value only within the directory it was recorded for.
pub fn module_key(name: &str, dir: &Path) -> String {
    let mut hasher = Sha1::new();
    for part in [name.as_bytes(), dir.as_os_str().as_encoded_bytes()] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher
        .finalize()
        .iter()
        .map(|&b| format!("{b:02x}"))
        .collect()
}

/// Read and deserialize a cached value, if a fresh entry exists and the cache is
/// enabled.
pub fn read<T: DeserializeOwned>(key: &str) -> Option<T> {
    if !cache_enabled() {
        return None;
    }

    let path = cache_dir().join(key);
    if !is_fresh(&path) {
        return None;
    }

    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Serialize and store a value in the cache, replacing any previous entry.
pub fn write<T: Serialize>(key: &str, value: &T) {
    if !cache_enabled() {
        return;
    }

    let dir = cache_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };

    // Write to a per-process temp file and rename so concurrent readers always
    // observe either the old or the new entry, never a partial write.
    let tmp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    if fs::write(&tmp, &bytes).is_ok() {
        let _ = fs::rename(&tmp, dir.join(key));
    }
}

/// Delete cache entries older than [`MAX_AGE`]. Called opportunistically by the
/// refresh so abandoned directories do not accumulate forever.
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
    fn module_key_distinguishes_name_and_dir() {
        assert_ne!(
            module_key("git_status", Path::new("/repo")),
            module_key("git_metrics", Path::new("/repo")),
        );
        assert_ne!(
            module_key("git_status", Path::new("/repo")),
            module_key("git_status", Path::new("/other")),
        );
    }

    #[test]
    fn read_returns_written_value() {
        let key = module_key("test_roundtrip", Path::new("/cache/test"));
        write(&key, &"hello".to_string());
        assert_eq!(read::<String>(&key), Some("hello".to_string()));
        let _ = fs::remove_file(cache_dir().join(key));
    }
}
