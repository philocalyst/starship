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
//!
//! The fish model under test: prompts paint with `--cached`; after each
//! command, `__starship_defer` (fired from `fish_postexec`) launches one
//! background `starship prompt --deferred --watch` whose poke lines twiddle
//! a per-session universal variable; the `--on-variable` handler repaints.

use super::{STARSHIP_BIN, ScratchEnv, pids_matching_with_cwd_under, substituted_init_script};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
        cmd.current_dir(root);
        for (k, v) in Self::isolated_envs(root) {
            cmd.env(k, v);
        }
        cmd.env("STARSHIP_CONFIG", &self.scratch.config_path);
        cmd.env("STARSHIP_CACHE", &self.scratch.cache_path);
        cmd
    }

    /// Run `fish -c <script>` inside a fresh isolated sandbox (its own
    /// throwaway `HOME`/`XDG_*` root), returning combined stdout+stderr.
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

    /// Snippet setting the standard prompt-context variables and launching a
    /// background watcher via the real `__starship_defer`, then blocking (via
    /// `kill -0` polling) until that watcher's forked block exits -- i.e.
    /// "fire a refresh and wait for it to finish". Only valid with the
    /// default `refresh_interval = 0`, where the watcher exits right after
    /// its refresh poke; a positive interval keeps it alive ticking (see
    /// [`watcher_lifetime_follows_refresh_interval`]).
    /// Does NOT source the init script itself (see [`Self::source_snippet`]);
    /// callers compose the two so a caller that needs to observe state
    /// between sourcing and firing (e.g. reading off a per-session variable
    /// the init script just assigned) can do so.
    fn fire_and_wait_snippet(&self) -> &'static str {
        r#"
set -g STARSHIP_CMD_STATUS 0
set -g STARSHIP_CMD_PIPESTATUS 0
set -g STARSHIP_KEYMAP insert
set -g STARSHIP_DURATION 0
set -g STARSHIP_JOBS 0
__starship_defer
set -l p $__starship_defer_pid
while kill -0 $p 2>/dev/null
    sleep 0.02
end
"#
    }

    /// Snippet sourcing the init script, then running
    /// [`Self::fire_and_wait_snippet`] -- the common "source, fire a
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

/// A cold prompt (empty cache) must render live output containing both
/// modules' real values: `--cached` with nothing recorded falls through and
/// computes everything live.
#[test]
fn cold_prompt_renders_live_output() {
    let env = FishEnv::new();
    env.reset_cache();

    let out = env.run(&format!("{}\nfish_prompt", env.source_snippet()));
    assert!(out.contains("FAST") && out.contains("SLOW"), "got: '{out}'");
}

/// A warm `--cached` prompt must reflect the recorded slow-module value
/// quickly, once a real background refresh (fired via the real
/// `__starship_defer`) has populated the cache.
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

/// `__starship_defer` must launch a genuine background watcher that
/// populates the on-disk cache and pokes this session's per-session
/// universal variable when the refresh lands.
#[test]
fn defer_populates_cache_and_pokes_repaint_variable() {
    let env = FishEnv::new();
    env.reset_cache();

    let root = env.path().join("session_defer_poke");
    std::fs::create_dir_all(&root).unwrap();
    let poke_log = root.join("poke.log");
    std::fs::write(&poke_log, "").unwrap();

    // A probe handler on the same per-session variable the init script's own
    // repaint handler watches, logging every poke delivery. The idle-poll
    // loop services fish's event loop so the universal-variable notification
    // can be delivered.
    let script = format!(
        r#"{source}
function __probe_poke --on-variable $__starship_defer_var
    echo POKE_DELIVERED >> '{log}'
end
{fire_and_wait}
for i in (seq 1 20)
    sleep 0.05
end
echo DONE
"#,
        source = env.source_snippet(),
        log = poke_log.display(),
        fire_and_wait = env.fire_and_wait_snippet(),
    );
    let out = env.run_in(&root, &script);
    let pokes = std::fs::read_to_string(&poke_log).unwrap_or_default();

    assert!(out.contains("DONE"), "got: '{out}'");
    assert!(
        env.cache_file_count() > 0,
        "expected at least one on-disk cache snapshot after a background refresh"
    );
    assert!(
        pokes.contains("POKE_DELIVERED"),
        "the watcher's refresh poke never twiddled this session's repaint variable; out: '{out}'"
    );
}

/// The `fish_postexec` hook is what fires the watcher in real sessions --
/// repaint-triggered `fish_prompt` re-runs must not fire anything (that's the
/// point of moving the firing out of `fish_prompt`). `emit fish_postexec`
/// exercises the real registered handler.
#[test]
fn postexec_fires_watcher_and_prompt_draw_does_not() {
    let env = FishEnv::new();
    env.reset_cache();

    let script = format!(
        r#"{source}
fish_prompt >/dev/null
set -q __starship_defer_pid; and echo 'FIRED_BY_PROMPT:yes'; or echo 'FIRED_BY_PROMPT:no'
emit fish_postexec 'true'
set -q __starship_defer_pid; and echo 'FIRED_BY_POSTEXEC:yes'; or echo 'FIRED_BY_POSTEXEC:no'
__starship_defer_kill
"#,
        source = env.source_snippet(),
    );
    let out = env.run(&script);

    assert!(
        out.contains("FIRED_BY_PROMPT:no"),
        "a bare prompt draw fired the watcher -- repaints would re-fire refreshes; out: '{out}'"
    );
    assert!(
        out.contains("FIRED_BY_POSTEXEC:yes"),
        "fish_postexec did not fire the watcher; out: '{out}'"
    );
}

/// The watcher job must be `disown`ed, keeping it out of `jobs` / `jobs -p`.
#[test]
fn watcher_job_is_disowned_from_jobs_list() {
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

/// Two concurrent, unrelated fish sessions must not leak repaint-variable
/// state into each other. Regression test for the cross-session
/// universal-variable leak bug.
///
/// Both sessions must share the same `XDG_CONFIG_HOME` (that's where fish's
/// universal variable file, and thus any leak, would be observed) but must
/// otherwise be unrelated -- session B never fires its own refresh. Run
/// "concurrently" by spawning both as child processes and interleaving:
/// start B first (idle, polling), let it settle, then run A's refresh to
/// completion while B is still alive, then join B.
#[test]
fn concurrent_sessions_do_not_leak_repaint_variable_state() {
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
    // its own per-session poke variable. It never fires a watcher itself.
    let session_b_script = format!(
        r#"{source}
echo "SESSION_B_OWN_VAR:$__starship_defer_var"
function __probe_leak --on-variable $__starship_defer_var
    echo "LEAK_HANDLER_FIRED value=$$__starship_defer_var" >> '{log}'
end
for i in (seq 1 50)
    sleep 0.05
end
echo SESSION_B_DONE
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
    // pid-scoped) poke-var name it assigned, then fires a real refresh
    // mid-way through session B's lifetime -- sharing the same
    // XDG_CONFIG_HOME (and thus the same universal variable file) but
    // otherwise a wholly separate fish process/session.
    let session_a_script = format!(
        r#"{source}
echo "SESSION_A_OWN_VAR:$__starship_defer_var"
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

    assert!(
        !session_a_var.is_empty() && !session_b_var.is_empty(),
        "could not determine per-session poke-var names (A='{session_a_var}' B='{session_b_var}')"
    );
    assert_ne!(
        session_a_var, session_b_var,
        "sessions ended up with the SAME poke-var name ('{session_a_var}') -- not isolated"
    );
    assert!(
        leak_log_contents.trim().is_empty(),
        "session B's handler fired due to session A's refresh: {leak_log_contents}"
    );
}

/// `STARSHIP_ASYNC=0` must reproduce the old fully synchronous behavior:
/// no `--cached` flag installed, no handlers engaged, no watcher fired even
/// through a real `fish_postexec`.
#[test]
fn starship_async_zero_disables_all_async_machinery() {
    let env = FishEnv::new();
    env.reset_cache();

    let script = format!(
        r#"{}
set -q __starship_cached; and echo 'CACHED_FLAG:set'; or echo 'CACHED_FLAG:unset'
functions -q __starship_defer_repaint; and echo 'HANDLER_DEFINED:yes'; or echo 'HANDLER_DEFINED:no'
functions -q __starship_defer_cleanup; and echo 'CLEANUP_DEFINED:yes'; or echo 'CLEANUP_DEFINED:no'
functions -q __starship_defer_postexec; and echo 'POSTEXEC_DEFINED:yes'; or echo 'POSTEXEC_DEFINED:no'
fish_prompt >/dev/null
emit fish_postexec 'true'
set -q __starship_defer_pid; and echo 'FIRED:yes'; or echo 'FIRED:no'
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

    assert!(out.contains("CACHED_FLAG:unset"), "got: '{out}'");
    assert!(out.contains("HANDLER_DEFINED:no"), "got: '{out}'");
    assert!(out.contains("CLEANUP_DEFINED:no"), "got: '{out}'");
    assert!(out.contains("POSTEXEC_DEFINED:no"), "got: '{out}'");
    assert!(out.contains("FIRED:no"), "got: '{out}'");
    // Direct mode never touches the cache either.
    assert_eq!(
        env.cache_file_count(),
        0,
        "STARSHIP_ASYNC=0 wrote cache entries"
    );
}

/// The live-update tick lives inside the watcher (`--watch` + the configured
/// `refresh_interval`), not in shell-side timers: with `refresh_interval = 0`
/// the watcher exits right after its refresh poke, and with a positive value
/// it stays alive ticking (poking the same variable) until killed.
#[test]
fn watcher_lifetime_follows_refresh_interval() {
    let env = FishEnv::new();

    for (interval, expect_alive) in [("0", false), ("1", true)] {
        let root = env.path().join(format!("session_tick_{interval}"));
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join("starship.toml");
        std::fs::write(
            &cfg,
            format!("refresh_interval = {interval}\nformat = \"$character\"\n"),
        )
        .unwrap();
        let tick_log = root.join("tick.log");
        std::fs::write(&tick_log, "").unwrap();

        let script = format!(
            r#"{source}
function __probe_tick --on-variable $__starship_defer_var
    echo TICKED >> '{log}'
end
set -g STARSHIP_CMD_STATUS 0
set -g STARSHIP_CMD_PIPESTATUS 0
set -g STARSHIP_KEYMAP insert
set -g STARSHIP_DURATION 0
set -g STARSHIP_JOBS 0
__starship_defer
# Service the event loop long enough for the refresh (fast: $character only)
# and, when configured, at least one 1s tick to land.
for i in (seq 1 50)
    sleep 0.05
end
kill -0 $__starship_defer_pid 2>/dev/null; and echo 'WATCHER:alive'; or echo 'WATCHER:gone'
__starship_defer_kill
set -q __starship_defer_pid; and echo 'AFTER_KILL_PID:set'; or echo 'AFTER_KILL_PID:unset'
"#,
            source = env.source_snippet(),
            log = tick_log.display(),
        );
        let mut cmd = env.command_in(&root, &script);
        cmd.env("STARSHIP_CONFIG", &cfg);
        let output = cmd.output().expect("failed to run fish -c");
        let out = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        let ticks = std::fs::read_to_string(&tick_log).unwrap_or_default();
        let poke_count = ticks.lines().filter(|l| l.contains("TICKED")).count();

        if expect_alive {
            assert!(
                out.contains("WATCHER:alive"),
                "refresh_interval={interval}: watcher should stay alive ticking; out: '{out}'"
            );
            assert!(
                poke_count >= 2,
                "refresh_interval={interval}: expected the refresh poke plus at least one tick poke (saw {poke_count}); out: '{out}'"
            );
        } else {
            assert!(
                out.contains("WATCHER:gone"),
                "refresh_interval={interval}: watcher should exit right after its refresh poke; out: '{out}'"
            );
        }
        assert!(
            out.contains("AFTER_KILL_PID:unset"),
            "__starship_defer_kill did not clear the watcher pid; out: '{out}'"
        );
    }
}

/// Regression test for a real bug found in this exact rework: `__starship_defer`
/// used to background its work as an inline `begin ... | while read ...; end &`
/// block. Confirmed by direct testing that fish cannot actually kill that
/// construct as a unit -- `kill $last_pid` on it is a no-op, so with a
/// positive `refresh_interval` (a genuinely long-lived, ticking watcher) the
/// underlying `starship prompt --deferred --watch` process leaked forever,
/// once per command executed, never reaped by `__starship_defer_kill`. The
/// fix launches a genuine external `fish -c` process instead (context passed
/// via `env`), which fish CAN kill as a unit.
///
/// `AFTER_KILL_PID:unset` alone (checked in
/// [`watcher_lifetime_follows_refresh_interval`]) does NOT catch this: fish
/// clears its own `$__starship_defer_pid` bookkeeping variable regardless of
/// whether the `kill` call actually terminated anything. This test instead
/// polls the real OS process table (scoped by cwd, see
/// `pids_matching_with_cwd_under`) for the real `starship prompt --deferred
/// --watch` process and asserts it is actually gone after
/// `__starship_defer_kill` -- proving the underlying process died, not just
/// that fish's tracking variable was cleared.
#[test]
fn defer_kill_actually_terminates_the_watcher_process() {
    let env = FishEnv::new();
    let root = env.path().join("session_kill_terminates");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = root.join("starship.toml");
    // A positive refresh_interval makes the watcher genuinely long-lived
    // (ticking forever until killed), which is exactly the case that leaked.
    std::fs::write(&cfg, "refresh_interval = 1\nformat = \"$character\"\n").unwrap();

    let script = format!(
        r#"{source}
set -g STARSHIP_CMD_STATUS 0
set -g STARSHIP_CMD_PIPESTATUS 0
set -g STARSHIP_KEYMAP insert
set -g STARSHIP_DURATION 0
set -g STARSHIP_JOBS 0
__starship_defer
# Let the refresh (and, since refresh_interval=1, at least the start of
# ticking) actually begin before we try to kill it.
for i in (seq 1 20)
    sleep 0.05
end
__starship_defer_kill
echo KILL_ISSUED
"#,
        source = env.source_snippet(),
    );
    let mut cmd = env.command_in(&root, &script);
    cmd.env("STARSHIP_CONFIG", &cfg);
    let output = cmd.output().expect("failed to run fish -c");
    let out = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(out.contains("KILL_ISSUED"), "script did not complete; out: '{out}'");

    // The real watcher process is spawned with the fish session's cwd (see
    // FishEnv::command_in), so it's scoped the same way other leak checks in
    // this suite scope theirs.
    let pattern = format!("{} prompt --deferred --watch", STARSHIP_BIN.display());

    // Poll rather than assert instantly: even a correctly-killed process can
    // take a moment to actually leave the process table. But it must be gone
    // well within a couple of refresh_interval ticks -- if it's still there
    // after that, __starship_defer_kill did not actually terminate it (the
    // regression this test exists to catch), not merely "hasn't exited yet".
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut alive = pids_matching_with_cwd_under(&pattern, &root);
    while !alive.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        alive = pids_matching_with_cwd_under(&pattern, &root);
    }
    assert!(
        alive.is_empty(),
        "__starship_defer_kill did not actually terminate the watcher process: still running: {alive:?}"
    );
}

/// Regression test for a real bug reported by a user: `__starship_defer`
/// spawns its watcher via `env ... fish -c '...'`, which requires a bare
/// `fish` to resolve through `$PATH`. That is not a given -- e.g. `nix run
/// nixpkgs#fish` does not necessarily put `fish` itself on `$PATH` for child
/// processes -- and when it fails to resolve, `env` errors out silently (no
/// error surfaces to the user) and the watcher simply never starts: the cache
/// never gets populated or refreshed, and nothing ever updates. The fix
/// resolves the *currently running* fish's own absolute path via
/// `$__fish_bin_dir` (a long-standing internal fish variable) instead of
/// relying on `$PATH` at all, so this must work even when a bare `fish`
/// cannot be found.
#[test]
fn defer_starts_even_when_plain_fish_is_not_on_path() {
    let env = FishEnv::new();
    env.reset_cache();

    let root = env.path().join("session_no_fish_on_path");
    std::fs::create_dir_all(&root).unwrap();

    let script = format!(
        r#"{source}
# Simulate a shell where a bare `fish` does not resolve via $PATH (as with
# `nix run nixpkgs#fish`, which was the real report this test guards
# against), while keeping the tools the *test* itself still needs.
set -gx PATH /usr/bin /bin
set -g STARSHIP_CMD_STATUS 0
set -g STARSHIP_CMD_PIPESTATUS 0
set -g STARSHIP_KEYMAP insert
set -g STARSHIP_DURATION 0
set -g STARSHIP_JOBS 0
__starship_defer
set -l p $__starship_defer_pid
while kill -0 $p 2>/dev/null
    sleep 0.02
end
echo DONE
"#,
        source = env.source_snippet(),
    );
    // Resolve the real fish binary's absolute path *before* restricting
    // $PATH below, and launch it directly by that path -- `command_in` uses
    // `Command::new("fish")`, which itself needs to find fish via $PATH, so
    // reusing it here (with a restricted child PATH) would risk failing to
    // launch the outer session at all rather than testing the real scenario
    // (the outer fish already running; only the *bare `fish` lookup made
    // from inside that session* -- by `__starship_defer` -- should fail).
    let fish_path = Command::new("command")
        .args(["-v", "fish"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
        .expect("could not resolve a real fish binary to launch for this test");

    let mut cmd = Command::new(fish_path);
    cmd.arg("-c").arg(&script);
    cmd.current_dir(&root);
    for (k, v) in FishEnv::isolated_envs(&root) {
        cmd.env(k, v);
    }
    cmd.env("STARSHIP_CONFIG", &env.scratch.config_path);
    cmd.env("STARSHIP_CACHE", &env.scratch.cache_path);
    let output = cmd.output().expect("failed to run fish -c");
    let out = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);

    // "DONE" alone would also print if the watcher failed to spawn at all
    // (fish just wouldn't have set $__starship_defer_pid, so `kill -0 $p`
    // fails immediately and the loop "completes" trivially) -- the cache
    // actually being populated is the real proof that the watcher process
    // was spawned, ran the deferred refresh to completion, and exited on
    // its own, which is only possible if `env ... "$__fish_bin_dir/fish" -c`
    // successfully resolved and launched fish despite the restricted $PATH.
    assert!(out.contains("DONE"), "script did not complete; out: '{out}'");
    assert!(
        env.cache_file_count() > 0,
        "the watcher never populated the cache when a bare `fish` was not on $PATH -- \
         __starship_defer must resolve fish via $__fish_bin_dir, not $PATH; out: '{out}'"
    );
}
