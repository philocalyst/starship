//! Automated, self-contained regression test for the async prompt
//! integration in `src/init/starship.fish`. Drives the real, freshly built
//! `starship` binary via real `fish -c "..."` script invocations (fish's
//! own non-interactive scripting mode -- no readline/terminal interaction
//! is needed for any of these checks, since none of them exercise fish's
//! interactive line editor; `commandline -f repaint` is only *invoked*, its
//! effects are observed via universal-variable/cache/process-state side
//! effects, not by watching redrawn screen content), each run inside an
//! isolated scratch `HOME`/`XDG_*` sandbox so fish's universal variables
//! (which persist to disk in fish's own state dir) never touch the real
//! user's config or state.

use super::{ScratchEnv, substituted_init_script};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long the scratch config's slow `custom` module sleeps for. Comfortably
/// above `SLOW_MODULE_THRESHOLD` (5ms) but short enough that timing-sensitive
/// checks (warm cache read well under this) stay fast and non-flaky.
const SLOW_MODULE_MS: u64 = 200;

/// A scratch Fish environment: an isolated `HOME`/`XDG_*`/`STARSHIP_CONFIG`/
/// `STARSHIP_CACHE` sandbox (see [`ScratchEnv`]) plus the real
/// `starship.fish` init script with `::STARSHIP::` substituted for the real
/// built binary -- everything each check needs, built once per test. Mirrors
/// `BashEnv` in `bash.rs`.
struct FishEnv {
    scratch: ScratchEnv,
    init_script: PathBuf,
}

impl FishEnv {
    fn new() -> Self {
        let scratch = ScratchEnv::new(SLOW_MODULE_MS).expect("failed to set up scratch env");
        let init_script = scratch.path().join("starship_init.fish");
        std::fs::write(
            &init_script,
            substituted_init_script("src/init/starship.fish"),
        )
        .expect("failed to write starship_init.fish");
        Self {
            scratch,
            init_script,
        }
    }

    fn path(&self) -> &Path {
        self.scratch.path()
    }

    fn reset_cache(&self) {
        std::fs::remove_dir_all(&self.scratch.cache_path).ok();
        std::fs::create_dir_all(&self.scratch.cache_path).unwrap();
    }

    /// The actual on-disk cache entries live one level below
    /// `STARSHIP_CACHE`, at `STARSHIP_CACHE/cache/` (see
    /// `get_log_dir().join("cache")` in `src/cache.rs`), not directly inside
    /// the dir we point `STARSHIP_CACHE` at.
    fn cache_file_count(&self) -> usize {
        std::fs::read_dir(self.scratch.cache_path.join("cache"))
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter(|e| e.path().is_file())
                    .count()
            })
            .unwrap_or(0)
    }

    /// Isolated `HOME`/`XDG_*` env vars rooted at `root`, so universal
    /// variables, config, and cache never touch the real user's environment.
    /// Mirrors `isolated_env()` in the original shell-script test.
    fn isolated_envs(root: &Path) -> Vec<(String, String)> {
        for sub in [
            "home",
            "xdg_config/fish",
            "xdg_data",
            "xdg_state",
            "xdg_cache",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        vec![
            ("HOME".to_string(), root.join("home").display().to_string()),
            (
                "XDG_CONFIG_HOME".to_string(),
                root.join("xdg_config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                root.join("xdg_data").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                root.join("xdg_state").display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".to_string(),
                root.join("xdg_cache").display().to_string(),
            ),
        ]
    }

    /// Build a `fish -c <script>` command rooted at the isolated sandbox
    /// `root`, with `STARSHIP_CONFIG`/`STARSHIP_CACHE` pointed at this env's
    /// shared scratch config and cache dir.
    fn command_in(&self, root: &Path, script: &str) -> Command {
        let mut cmd = Command::new("fish");
        cmd.arg("-c").arg(script);
        for (k, v) in Self::isolated_envs(root) {
            cmd.env(k, v);
        }
        cmd.env("STARSHIP_CONFIG", &self.scratch.config_path);
        cmd.env("STARSHIP_CACHE", &self.scratch.cache_path);
        cmd
    }

    /// Run `fish -c <script>` inside a fresh isolated sandbox (its own
    /// throwaway `HOME`/`XDG_*` root), returning combined stdout+stderr.
    /// Mirrors `run_fish_isolated()` in the original test, for checks that
    /// don't need to share their sandbox root with another session.
    fn run(&self, script: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = self.scratch.dir.path().join(format!("session_{id}"));
        std::fs::create_dir_all(&root).unwrap();
        self.run_in(&root, script)
    }

    /// Like [`Self::run`], but inside a caller-provided sandbox root, so
    /// multiple calls can share the same `HOME`/`XDG_*` (and thus the same
    /// universal-variable file).
    fn run_in(&self, root: &Path, script: &str) -> String {
        let output = self
            .command_in(root, script)
            .output()
            .expect("failed to run fish -c");
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        combined
    }

    /// Snippet setting the standard prompt-context variables and firing a
    /// background refresh, then blocking (via `kill -0` polling, exactly
    /// like the original test) until that refresh's `fish -c` subprocess
    /// exits -- i.e. "fire an async refresh and wait for it to finish".
    /// Does NOT source the init script itself (see [`Self::source_snippet`]);
    /// callers compose the two so a caller that needs to observe state
    /// between sourcing and firing (e.g. reading off a per-session variable
    /// the init script just assigned) can do so.
    fn fire_and_wait_snippet(&self) -> &'static str {
        r#"
set STARSHIP_CMD_STATUS 0
set STARSHIP_CMD_PIPESTATUS 0
set STARSHIP_KEYMAP insert
set STARSHIP_DURATION 0
set STARSHIP_JOBS 0
__starship_fire_async $STARSHIP_CMD_STATUS $STARSHIP_CMD_PIPESTATUS $STARSHIP_KEYMAP $STARSHIP_DURATION $STARSHIP_JOBS
set -l p $__starship_async_pid
while kill -0 $p 2>/dev/null
    sleep 0.02
end
"#
    }

    /// Snippet sourcing the init script, then running
    /// [`Self::fire_and_wait_snippet`] -- the common "source, fire an async
    /// refresh, and wait for it to finish" pattern.
    fn source_fire_and_wait_snippet(&self) -> String {
        format!(
            "{}\n{}",
            self.source_snippet(),
            self.fire_and_wait_snippet()
        )
    }

    fn source_snippet(&self) -> String {
        format!("source '{}'", self.init_script.display())
    }
}

/// Sanity check that the `::STARSHIP::` template token in the init script
/// gets substituted with the real built binary's path.
#[test]
fn init_script_substitutes_starship_binary_path() {
    let init_fish_content = substituted_init_script("src/init/starship.fish");
    assert!(
        init_fish_content.contains(&super::STARSHIP_BIN.display().to_string()),
        "::STARSHIP:: substitution failed"
    );
}

/// Regression test for the `set -gx STARSHIP_ASYNC` (no value) bug: `set -gx
/// VAR` with no value argument erases VAR in fish, so this asserts
/// `STARSHIP_ASYNC` comes out exported as a non-empty `"1"` after sourcing.
#[test]
fn starship_async_exported_as_one_after_sourcing() {
    let env = FishEnv::new();
    let out = env.run(&format!(
        "{}\nenv | grep '^STARSHIP_ASYNC='",
        env.source_snippet()
    ));
    assert_eq!(out.trim(), "STARSHIP_ASYNC=1", "got: '{}'", out.trim());
}

/// A cold prompt (empty cache) must render live output containing both
/// modules' real values.
#[test]
fn cold_prompt_renders_live_output() {
    let env = FishEnv::new();
    env.reset_cache();

    let out = env.run(&format!("{}\nfish_prompt", env.source_snippet()));
    assert!(out.contains("FAST") && out.contains("SLOW"), "got: '{out}'");
}

/// A warm `CacheRead` prompt must reflect the cached slow-module value
/// quickly. The cache dir would already have entries from a live compute
/// (`CacheRead` mode writes-through on live compute of slow modules), but to
/// be sure the cache is actually populated via the *async refresh* path,
/// fire it explicitly and wait for completion before measuring the warm
/// read.
#[test]
fn warm_cache_read_reflects_cached_value_quickly() {
    let env = FishEnv::new();
    env.reset_cache();

    // `fish_prompt`'s rendered output can itself contain newlines (a leading
    // clear-to-end-of-screen escape on its own line, then the actual prompt
    // content) so it can't be recovered by splitting the script's stdout on
    // lines and matching an `OUT:` prefix on just one of them -- bracket it
    // between unique markers and extract everything in between instead
    // (mirrors the PS1 extraction in bash.rs's byte-for-byte check).
    let script = format!(
        "{}\nset -l start (date +%s.%N)\nset -l out (fish_prompt 2>&1 | string collect)\nset -l end (date +%s.%N)\nset -l elapsed_ms (math \"($end - $start) * 1000\")\nprintf 'OUT_START:%sOUT_END:\\n' \"$out\"\necho \"MS:$elapsed_ms\"",
        env.source_fire_and_wait_snippet()
    );
    let result = env.run(&script);
    let text = result
        .split_once("OUT_START:")
        .and_then(|(_, rest)| rest.split_once("OUT_END:"))
        .map(|(out, _)| out.to_string())
        .unwrap_or_default();
    let elapsed_ms: Option<f64> = result
        .lines()
        .find_map(|l| l.strip_prefix("MS:"))
        .and_then(|s| s.trim().parse().ok());

    assert!(
        text.contains("SLOW"),
        "expected cached SLOW text, got: '{text}'"
    );
    assert!(
        elapsed_ms.is_some_and(|ms| ms < 150.0),
        "expected a fast cached render (<150ms, well under the {SLOW_MODULE_MS}ms module sleep), took {elapsed_ms:?}ms"
    );
}

/// `__starship_fire_async` must launch a genuine background `fish -c`
/// subprocess that populates the on-disk cache.
#[test]
fn fire_async_populates_on_disk_cache() {
    let env = FishEnv::new();
    env.reset_cache();

    let script = format!("{}\necho DONE", env.source_fire_and_wait_snippet());
    let out = env.run(&script);

    assert!(out.contains("DONE"), "got: '{out}'");
    assert!(
        env.cache_file_count() > 0,
        "expected at least one on-disk cache entry after a background refresh"
    );
}

/// The async refresh job must be `disown`ed, keeping it out of `jobs` /
/// `jobs -p`.
#[test]
fn async_job_is_disowned_from_jobs_list() {
    let env = FishEnv::new();
    env.reset_cache();

    let script = format!(
        r#"{}
echo "JOBS_COUNT:"(jobs 2>/dev/null | count)
echo "JOBS_P_COUNT:"(jobs -p 2>/dev/null | count)
"#,
        env.source_fire_and_wait_snippet()
    );
    let out = env.run(&script);
    let jobs_count = out
        .lines()
        .find_map(|l| l.strip_prefix("JOBS_COUNT:"))
        .map(str::trim)
        .unwrap_or_default();
    let jobs_p_count = out
        .lines()
        .find_map(|l| l.strip_prefix("JOBS_P_COUNT:"))
        .map(str::trim)
        .unwrap_or_default();

    assert_eq!(jobs_count, "0", "full output: '{out}'");
    assert_eq!(jobs_p_count, "0", "full output: '{out}'");
}

/// Two concurrent, unrelated fish sessions must not leak repaint/ready-
/// variable state into each other. Regression test for the cross-session
/// universal-variable leak bug.
///
/// Both sessions must share the same `XDG_CONFIG_HOME` (that's where fish's
/// universal variable file, and thus any leak, would be observed) but must
/// otherwise be unrelated -- session B never fires its own refresh. Run
/// "concurrently" by spawning both as child processes and interleaving:
/// start B first (idle, polling), let it settle, then run A's refresh to
/// completion while B is still alive, then join B.
#[test]
fn concurrent_sessions_do_not_leak_ready_variable_state() {
    let env = FishEnv::new();
    env.reset_cache();

    let shared_root = env.path().join("session_shared_uvars");
    for sub in [
        "home",
        "xdg_config/fish",
        "xdg_data",
        "xdg_state",
        "xdg_cache",
    ] {
        std::fs::create_dir_all(shared_root.join(sub)).unwrap();
    }

    let session_b_log = env.path().join("session_b.log");
    std::fs::write(&session_b_log, "").unwrap();

    // Session B: sits idle in a polling loop (services fish's event loop so
    // it CAN receive uvar notifications if they leak) and logs any change to
    // ANY __starship_async_ready_* variable, plus its own skip_fire state.
    // It never calls __starship_fire_async itself.
    let session_b_script = format!(
        r#"{source}
echo "SESSION_B_OWN_VAR:$__starship_async_ready_var"
function __probe_leak --on-variable $__starship_async_ready_var
    echo "LEAK_HANDLER_FIRED value=$$__starship_async_ready_var" >> '{log}'
end
for i in (seq 1 50)
    sleep 0.05
end
echo "SESSION_B_FINAL_SKIP_FIRE:$__starship_async_skip_fire"
"#,
        source = env.source_snippet(),
        log = session_b_log.display(),
    );
    let mut b_cmd = env.command_in(&shared_root, &session_b_script);
    b_cmd.stdout(std::process::Stdio::piped());
    b_cmd.stderr(std::process::Stdio::piped());
    let b_child = b_cmd.spawn().expect("failed to spawn session B fish");

    std::thread::sleep(Duration::from_millis(500));

    // Session A: sources the init script, reads off the (per-session,
    // randomized) ready-var name it assigned, then fires a real async
    // refresh mid-way through session B's lifetime -- sharing the same
    // XDG_CONFIG_HOME (and thus the same universal variable file) but
    // otherwise a wholly separate fish process/session.
    let session_a_script = format!(
        r#"{source}
echo "SESSION_A_OWN_VAR:$__starship_async_ready_var"
{fire_and_wait}
echo SESSION_A_BG_DONE
"#,
        source = env.source_snippet(),
        fire_and_wait = env.fire_and_wait_snippet(),
    );
    let session_a_stdout = env.run_in(&shared_root, &session_a_script);

    let b_output = b_child
        .wait_with_output()
        .expect("failed to wait on session B fish");
    let session_b_stdout = String::from_utf8_lossy(&b_output.stdout).into_owned();
    let leak_log_contents = std::fs::read_to_string(&session_b_log).unwrap_or_default();

    let session_a_var = session_a_stdout
        .lines()
        .find_map(|l| l.strip_prefix("SESSION_A_OWN_VAR:"))
        .unwrap_or_default();
    let session_b_var = session_b_stdout
        .lines()
        .find_map(|l| l.strip_prefix("SESSION_B_OWN_VAR:"))
        .unwrap_or_default();
    let session_b_final_skip = session_b_stdout
        .lines()
        .find_map(|l| l.strip_prefix("SESSION_B_FINAL_SKIP_FIRE:"))
        .unwrap_or_default();

    assert!(
        !session_a_var.is_empty() && !session_b_var.is_empty(),
        "could not determine per-session ready-var names (A='{session_a_var}' B='{session_b_var}')"
    );
    assert_ne!(
        session_a_var, session_b_var,
        "sessions ended up with the SAME ready-var name ('{session_a_var}') -- not isolated"
    );
    assert!(
        leak_log_contents.trim().is_empty(),
        "session B's handler fired due to session A's refresh: {leak_log_contents}"
    );
    assert_eq!(
        session_b_final_skip, "0",
        "session B's skip_fire was perturbed (expected '0'); full B output: '{session_b_stdout}'"
    );
}

/// `STARSHIP_ASYNC=0` must reproduce the old fully synchronous behavior,
/// with no async functions/handlers engaged and no background job fired.
#[test]
fn starship_async_zero_disables_all_async_machinery() {
    let env = FishEnv::new();
    env.reset_cache();

    let script = format!(
        r#"{}
echo "EXPORTED:$STARSHIP_ASYNC"
functions -q __starship_async_repaint; and echo 'HANDLER_DEFINED:yes'; or echo 'HANDLER_DEFINED:no'
functions -q __starship_async_cleanup; and echo 'CLEANUP_DEFINED:yes'; or echo 'CLEANUP_DEFINED:no'
fish_prompt >/dev/null
set -q __starship_async_pid; and echo 'FIRED:yes'; or echo 'FIRED:no'
"#,
        env.source_snippet()
    );
    let root = env.path().join("session_async_disabled");
    std::fs::create_dir_all(&root).unwrap();
    let mut cmd = env.command_in(&root, &script);
    cmd.env("STARSHIP_ASYNC", "0");
    let output = cmd.output().expect("failed to run fish -c");
    let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&output.stderr));

    assert!(out.contains("EXPORTED:0"), "got: '{out}'");
    assert!(out.contains("HANDLER_DEFINED:no"), "got: '{out}'");
    assert!(out.contains("CLEANUP_DEFINED:no"), "got: '{out}'");
    assert!(out.contains("FIRED:no"), "got: '{out}'");
}
