//! Optional cache invalidation through an already-running Facebook Watchman.
//!
//! Starship is a Watchman *client*, never its owner: every request uses
//! `--no-spawn`, and no shell script subscribes to or launches a service.
//! Watchman answers the otherwise-expensive question "did anything in this
//! tree change?"; an unavailable, slow, or reset service always falls back to
//! the ordinary dependency checks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Environment switch for using a pre-existing Watchman service.
const ENABLED: &str = "STARSHIP_WATCHMAN";
const PROMPT_BUDGET: Duration = Duration::from_millis(20);
const REFRESH_BUDGET: Duration = Duration::from_millis(500);

/// The amount of time this operation may spend asking Watchman.
///
/// Prompt paints are deliberately tiny; a refresh is already off the critical
/// paint path and can afford a short synchronization wait. Both are bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Prompt,
    Refresh,
}

impl Budget {
    fn duration(self) -> Duration {
        match self {
            Self::Prompt => PROMPT_BUDGET,
            Self::Refresh => REFRESH_BUDGET,
        }
    }

    fn milliseconds(self) -> u64 {
        self.duration().as_millis() as u64
    }
}

/// A point in Watchman's event stream for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    root: PathBuf,
    clock: String,
}

/// The change history Watchman can safely establish after a checkpoint.
///
/// `All` means Watchman restarted or recrawled and cannot prove the old clock
/// continuous. It is intentionally an invalidation, never permission to reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSet {
    Clean,
    Paths(Vec<PathBuf>),
    All,
}

impl ChangeSet {
    /// Whether an event can affect a dependency that represents a whole tree.
    pub fn touches_tree(&self, tree: &Path) -> bool {
        match self {
            Self::Clean => false,
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|path| path.starts_with(tree)),
        }
    }
}

/// Record the current clock for `path` when an existing Watchman service is
/// available. No service is spawned; failure is intentionally indistinguishable
/// from Watchman being disabled so cache correctness never depends on it.
pub fn checkpoint(path: &Path, budget: Budget) -> Option<Checkpoint> {
    if !enabled() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    let project = command(json!(["watch-project", path]), budget).ok()?;
    let root: PathBuf = project.get("watch")?.as_str()?.into();
    // A clock by itself may be behind pending filesystem notifications. Asking
    // Watchman to synchronize makes this an actual before-render boundary.
    let clock = command(
        json!(["clock", root, { "sync_timeout": budget.milliseconds() }]),
        budget,
    )
    .ok()?
    .get("clock")?
    .as_str()?
    .into();
    Some(Checkpoint { root, clock })
}

/// Return all events since `checkpoint`, or `None` if Watchman cannot make a
/// timely, trustworthy statement. Callers must then use dependency stamps.
pub fn changes(checkpoint: &Checkpoint, budget: Budget) -> Option<ChangeSet> {
    if !enabled() {
        return None;
    }
    let response = command(
        json!([
            "query",
            checkpoint.root,
            {
                "since": checkpoint.clock,
                "fields": ["name"],
                "sync_timeout": budget.milliseconds(),
                "lock_timeout": budget.milliseconds(),
            }
        ]),
        budget,
    )
    .ok()?;
    if response
        .get("is_fresh_instance")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(ChangeSet::All);
    }
    let files = response.get("files")?.as_array()?;
    let paths = files
        .iter()
        .map(|file| {
            file.get("name")?
                .as_str()
                .map(|name| checkpoint.root.join(name))
        })
        .collect::<Option<Vec<_>>>()?;
    if paths.is_empty() {
        Some(ChangeSet::Clean)
    } else {
        Some(ChangeSet::Paths(paths))
    }
}

fn enabled() -> bool {
    std::env::var_os(ENABLED).is_some_and(|value| value != "" && value != "0")
}

/// Issue one JSON command to an existing Watchman service. `--no-spawn` is the
/// lifecycle boundary: Watchman remains entirely outside Starship.
fn command(request: Value, budget: Budget) -> Result<Value, ()> {
    let mut child = Command::new("watchman")
        .args(["--no-spawn", "-j", "--no-pretty"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    if let Some(mut stdin) = child.stdin.take() {
        serde_json::to_writer(&mut stdin, &request).map_err(|_| ())?;
        stdin.write_all(b"\n").map_err(|_| ())?;
    } else {
        return Err(());
    }

    let start = Instant::now();
    loop {
        if child.try_wait().map_err(|_| ())?.is_some() {
            let output = child.wait_with_output().map_err(|_| ())?;
            if !output.status.success() {
                return Err(());
            }
            let response: Value = serde_json::from_slice(&output.stdout).map_err(|_| ())?;
            return if response.get("error").is_some() {
                Err(())
            } else {
                Ok(response)
            };
        }
        if start.elapsed() >= budget.duration() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_round_trips() {
        let checkpoint = Checkpoint {
            root: PathBuf::from("/repo"),
            clock: String::from("c:1:2"),
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        assert_eq!(
            serde_json::from_str::<Checkpoint>(&json).unwrap(),
            checkpoint
        );
    }

    #[test]
    fn change_sets_only_invalidate_touched_trees() {
        let changes = ChangeSet::Paths(vec![PathBuf::from("/repo/src/lib.rs")]);
        assert!(changes.touches_tree(Path::new("/repo")));
        assert!(changes.touches_tree(Path::new("/repo/src")));
        assert!(!changes.touches_tree(Path::new("/other")));
        assert!(!ChangeSet::Clean.touches_tree(Path::new("/repo")));
        assert!(ChangeSet::All.touches_tree(Path::new("/repo")));
    }
}
