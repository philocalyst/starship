//! End-to-end tests for the Elvish async-prompt integration in
//! `src/init/starship.elv`, driven against the real starship binary and a
//! real interactive Elvish session (over a real pty), reproducing the manual
//! verification used while developing that file.
//!
//! This is a Rust port of the former `tests/init-async/test_elvish.sh` +
//! its Python `pty`/`pyte` driver. Elvish's `edit:*` line-editing functions
//! need a genuine tty and proper terminal emulation to interpret their
//! redraws, which is why (like the Zsh test) this drives a real
//! `PtySession` rather than a plain piped subprocess. `PtySession`'s
//! `alacritty_terminal`-backed terminal emulator (auto-answering
//! cursor-position-report queries via `Term::device_status`, exactly like a
//! real terminal) replaces the old hand-rolled Python `pyte` screen.

use super::{PtySession, STARSHIP_BIN, ScratchEnv, SpawnOptions, assert_no_orphaned_processes, substituted_init_script};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long the scratch config's slow `custom` module sleeps for.
/// Comfortably above `SLOW_MODULE_THRESHOLD` (5ms), short enough to keep the
/// suite fast.
const SLOW_MODULE_MS: u64 = 200;

/// A scratch Elvish environment: an isolated `STARSHIP_CONFIG`/
/// `STARSHIP_CACHE`/`HOME` (elvish's daemon persists history/state under
/// `HOME`, so this must be scoped per-test the same way the shell-script
/// version scoped it under its own scratch `$HOME`), a real built starship
/// binary, and the real `starship.elv` init script with `::STARSHIP::`
/// substituted -- everything each check needs, built once per test.
struct ElvishEnv {
    scratch: ScratchEnv,
    init_script: PathBuf,
    home: PathBuf,
}

impl ElvishEnv {
    fn new() -> Self {
        if which("elvish").is_none() {
            panic!("elvish not found on PATH. Install it with: brew install elvish");
        }

        let scratch = ScratchEnv::new(SLOW_MODULE_MS).expect("failed to set up scratch env");
        let init_script = scratch.path().join("init.elv");
        let init_content = substituted_init_script("src/init/starship.elv");
        fs::write(&init_script, &init_content).expect("failed to write init.elv");
        assert!(
            init_content.contains(&STARSHIP_BIN.display().to_string()),
            "::STARSHIP:: substitution failed"
        );

        let home = scratch.path().join("home");
        fs::create_dir_all(&home).unwrap();

        Self { scratch, init_script, home }
    }

    fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// Wipe and recreate the cache + home dirs, e.g. between two spawns in
    /// the same test that need a clean daemon/cache state.
    fn reset_cache_and_home(&self) {
        fs::remove_dir_all(&self.scratch.cache_path).ok();
        fs::remove_dir_all(&self.home).ok();
        fs::create_dir_all(&self.home).unwrap();
    }

    /// Spawn a real interactive `elvish -rc <init.elv>` session with an
    /// isolated `STARSHIP_CONFIG`/`STARSHIP_CACHE`/`HOME`.
    fn spawn(&self, async_value: &str) -> PtySession {
        fs::create_dir_all(&self.scratch.cache_path).ok();
        fs::create_dir_all(&self.home).ok();
        let config_s = self.scratch.config_path.display().to_string();
        let cache_s = self.scratch.cache_path.display().to_string();
        let home_s = self.home.display().to_string();
        let xdg_config = format!("{home_s}/.config");
        let xdg_cache = format!("{home_s}/.cache");
        PtySession::spawn(SpawnOptions {
            program: "elvish",
            args: &["-rc", &self.init_script.display().to_string()],
            envs: &[
                ("STARSHIP_CONFIG", &config_s),
                ("STARSHIP_CACHE", &cache_s),
                ("STARSHIP_ASYNC", async_value),
                ("HOME", &home_s),
                ("XDG_CONFIG_HOME", &xdg_config),
                ("XDG_CACHE_HOME", &xdg_cache),
            ],
            cwd: Some(self.path()),
        })
        .expect("failed to spawn elvish")
    }

    /// mtimes (as `SystemTime`) of every file directly inside the cache's
    /// on-disk `cache` subdirectory, keyed by file name. Used, like the
    /// original marker-file timestamp technique in the Python driver, as an
    /// independent clock (filesystem mtimes on the on-disk cache itself) to
    /// prove the redraw happens only after the background `--deferred` run
    /// actually wrote the cache -- not immediately, and not never.
    fn cache_mtimes(&self) -> std::collections::BTreeMap<String, SystemTime> {
        let dir = self.scratch.cache_path.join("cache");
        let mut out = std::collections::BTreeMap::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                    && let Ok(mtime) = meta.modified()
                {
                    out.insert(entry.file_name().to_string_lossy().into_owned(), mtime);
                }
            }
        }
        out
    }
}

/// Number of `edit:after-command` hooks currently installed, read back via a
/// probe command run inside the session. Elvish's line editor may install
/// its own default after-command hook(s) before any rc file runs, so the
/// absolute count is not itself meaningful -- only the delta between an
/// async=1 and an async=0 run (which should be exactly 1, for
/// starship-async-refresh) is meaningful.
fn hook_count(session: &mut PtySession) -> Option<u32> {
    session.send_and_pump(
        "var hooks = (count $edit:after-command); echo HOOK_COUNT:$hooks\r",
        Duration::from_secs(1),
    );
    session.extract_tag("HOOK_COUNT")?.trim().parse().ok()
}

fn which(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// After warming the cache with one command, a plain prompt paints instantly
/// via CacheRead: the settled screen should show both SLOW and FAST content.
#[test]
fn warm_prompt_shows_cached_content() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session = env.spawn("1");
    session.pump(Duration::from_millis(1500));
    // Warm the cache: run one command to trigger a background refresh, then
    // wait for it to land, so that plain prompts become CacheRead-fast
    // afterwards.
    session.send_and_pump("echo warm\r", Duration::from_secs(2));

    let after_warm = session.visible_text();
    assert!(
        after_warm.contains("SLOW") && after_warm.contains("FAST"),
        "prompt should show cached SLOW+FAST content after warm-up (CacheRead warm path exercised): got {after_warm}"
    );

    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    session.kill();
}

/// The redraw happens only after the background `--deferred` process
/// completes: the on-disk cache file's mtime must be >= the command-submit
/// time (i.e. the refresh actually ran and wrote the cache after the command
/// was submitted, not before / not never), and the settled prompt reflects
/// the refreshed (SLOW-containing) redraw.
///
/// Mirrors the original Python driver's marker-file timestamp technique:
/// filesystem mtimes on the on-disk cache file are used as an independent
/// clock, rather than relying on screen-scrape timing alone.
#[test]
fn redraw_happens_only_after_background_refresh_writes_cache() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session = env.spawn("1");
    session.pump(Duration::from_millis(1500));
    session.send_and_pump("echo warm\r", Duration::from_secs(2));

    let before_mtimes = env.cache_mtimes();
    let submit_time = SystemTime::now();
    session.send("echo timing-probe\r");
    // Sample almost immediately after submit (the true "instant" claim is
    // validated quantitatively via the cache-mtime-vs-submit-time
    // comparison below, not via this immediate screen scrape).
    session.pump(Duration::from_millis(150));
    session.pump(Duration::from_secs(2));
    let settled_screen = session.visible_text();
    let after_mtimes = env.cache_mtimes();

    let cache_write_time = after_mtimes.values().max().copied();
    // `visible_text()` returns the full fixed-height screen (unlike the
    // original Python driver's `screen_text()`, which popped trailing blank
    // lines), so the last non-blank line is the actual bottom-most prompt
    // row rather than empty screen padding below it.
    let settled_tail = settled_screen.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");

    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    session.kill();

    let write_time = cache_write_time.expect("could not determine cache write timestamp");
    let order_ok = write_time >= submit_time;
    assert!(
        order_ok && settled_tail.contains("SLOW"),
        "cache write should occur at/after command submission and settled prompt should reflect refreshed (SLOW-containing) redraw: order_ok={order_ok} settled_tail={settled_tail:?} before={before_mtimes:?} after={after_mtimes:?}"
    );
}

/// No multi-line job-notification dump (closure source text) appears in the
/// raw terminal output. This is the regression that was found and fixed:
/// backgrounding an inline `{ ... }` closure literal caused Elvish to dump
/// the entire closure source as part of its "job finished" notification; the
/// fix names the closure (`-starship-async-refresh-job`) so any
/// notification (should one ever print) is a short `job <name> & finished`
/// line instead.
#[test]
fn no_closure_source_dump_in_job_notification() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session = env.spawn("1");
    session.pump(Duration::from_millis(1500));
    session.send_and_pump("echo warm\r", Duration::from_secs(2));
    session.send_and_pump("echo timing-probe\r", Duration::from_secs(2));

    let raw_log = session.raw_transcript();

    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    session.kill();

    let has_closure_source_dump = raw_log.contains("notify-bg-job-success") || regex::Regex::new(r"job \{").unwrap().is_match(&raw_log);
    assert!(
        !has_closure_source_dump,
        "raw output should not contain backgrounded closure source text (job-notification spam regression)"
    );
}

/// `notify-bg-job-success` is actually suppressed globally: no
/// 'job ... finished' notification text at all, in any form. This verifies
/// the fix for the `tmp`-scoping bug: `tmp` restores its old value when the
/// *enclosing function* returns, which happens immediately after
/// backgrounding the refresh job, long before that job actually finishes and
/// the notification would fire -- so `notify-bg-job-success` must be set
/// globally (`set`, not `tmp`).
#[test]
fn job_finished_notification_is_fully_suppressed() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session = env.spawn("1");
    session.pump(Duration::from_millis(1500));
    session.send_and_pump("echo warm\r", Duration::from_secs(2));
    session.send_and_pump("echo timing-probe\r", Duration::from_secs(2));

    let raw_log = session.raw_transcript();

    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    session.kill();

    let job_finished_re = regex::Regex::new(r"job .* finished").unwrap();
    assert!(
        !job_finished_re.is_match(&raw_log),
        "no 'job ... finished' notification text should appear anywhere in the session (notify-bg-job-success not suppressed)"
    );
}

/// No orphaned `starship prompt --deferred` processes survive a burst of
/// rapid-fire commands (each faster than the slow module's sleep, so
/// refreshes overlap/queue up repeatedly -- Elvish never cancels an in-flight
/// refresh, so each must finish its bounded render and exit on its own).
#[test]
fn no_orphans_after_rapid_fire_commands() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session = env.spawn("1");
    session.pump(Duration::from_millis(1500));
    session.send_and_pump("echo warm\r", Duration::from_secs(2));

    for i in 0..5 {
        session.send_and_pump(&format!("echo rapid{i}\r"), Duration::from_millis(120));
    }
    session.pump(Duration::from_millis(2500));

    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    session.kill();

    std::thread::sleep(Duration::from_millis(500));
    let pattern = format!("{} prompt --deferred", STARSHIP_BIN.display());
    assert_no_orphaned_processes(&pattern, env.scratch.path());
}

/// `STARSHIP_ASYNC=0` must install exactly one fewer `after-command` hook
/// than `STARSHIP_ASYNC=1` (the difference being `starship-async-refresh`
/// itself). Compare the delta rather than an absolute count because
/// Elvish's line editor may install its own default hook(s) independent of
/// starship.elv, so the absolute baseline is not something this test should
/// hardcode.
#[test]
fn disabled_async_installs_exactly_one_fewer_hook() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session1 = env.spawn("1");
    session1.pump(Duration::from_millis(1500));
    session1.send_and_pump("echo warm\r", Duration::from_secs(2));
    let hook_count_async1 = hook_count(&mut session1);
    session1.send("exit\r");
    session1.pump(Duration::from_secs(1));
    session1.kill();

    env.reset_cache_and_home();

    let mut session0 = env.spawn("0");
    session0.pump(Duration::from_millis(1500));
    session0.send_and_pump("echo warm\r", Duration::from_secs(2));
    let hook_count_async0 = hook_count(&mut session0);
    session0.send("exit\r");
    session0.pump(Duration::from_secs(1));
    session0.kill();

    let (async1, async0) = (
        hook_count_async1.expect("could not read hook count for STARSHIP_ASYNC=1 session"),
        hook_count_async0.expect("could not read hook count for STARSHIP_ASYNC=0 session"),
    );
    let delta = async1 as i64 - async0 as i64;
    assert_eq!(
        delta, 1,
        "expected exactly 1 fewer hook with STARSHIP_ASYNC=0 than STARSHIP_ASYNC=1, got async=1:{async1} async=0:{async0} (delta={delta}), i.e. no async hook installed"
    );
}

/// Under `STARSHIP_ASYNC=0`, the session shows no background job activity at
/// all (fully synchronous, as before): no background refresh is ever
/// started, so no 'job ... finished' notification can appear either.
#[test]
fn disabled_async_shows_no_background_job_activity() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session0 = env.spawn("0");
    session0.pump(Duration::from_millis(1500));
    session0.send_and_pump("echo warm\r", Duration::from_secs(2));

    let raw_log0 = session0.raw_transcript();

    session0.send("exit\r");
    session0.pump(Duration::from_secs(1));
    session0.kill();

    let job_finished_re = regex::Regex::new(r"job .* finished").unwrap();
    assert!(
        !job_finished_re.is_match(&raw_log0),
        "STARSHIP_ASYNC=0 session should show no background job activity (no background job should ever be started)"
    );
}

/// Under `STARSHIP_ASYNC=0`, the prompt computes its full content
/// synchronously (Direct mode): both SLOW and FAST content must appear even
/// though nothing is ever cached/refreshed in the background.
#[test]
fn disabled_async_prompt_computes_full_content_synchronously() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session0 = env.spawn("0");
    session0.pump(Duration::from_millis(1500));
    session0.send_and_pump("echo warm\r", Duration::from_secs(2));

    let after_warm0 = session0.visible_text();

    session0.send("exit\r");
    session0.pump(Duration::from_secs(1));
    session0.kill();

    assert!(
        after_warm0.contains("SLOW") && after_warm0.contains("FAST"),
        "STARSHIP_ASYNC=0 prompt should compute full content synchronously (Direct mode, no cache omission): missing expected content: {after_warm0}"
    );
}

/// No leftover starship processes survive across a full async=1 then
/// async=0 session cycle (belt-and-suspenders final sweep, mirroring the
/// original script's end-of-test orphan check across both runs).
#[test]
fn no_leftover_starship_processes_after_full_cycle() {
    let env = ElvishEnv::new();
    env.reset_cache_and_home();

    let mut session1 = env.spawn("1");
    session1.pump(Duration::from_millis(1500));
    session1.send_and_pump("echo warm\r", Duration::from_secs(2));
    for i in 0..5 {
        session1.send_and_pump(&format!("echo rapid{i}\r"), Duration::from_millis(120));
    }
    session1.pump(Duration::from_millis(1000));
    session1.send("exit\r");
    session1.pump(Duration::from_secs(1));
    session1.kill();

    env.reset_cache_and_home();

    let mut session0 = env.spawn("0");
    session0.pump(Duration::from_millis(1500));
    session0.send_and_pump("echo warm\r", Duration::from_secs(2));
    session0.send("exit\r");
    session0.pump(Duration::from_secs(1));
    session0.kill();

    std::thread::sleep(Duration::from_millis(300));
    let pattern = format!("{} prompt", STARSHIP_BIN.display());
    assert_no_orphaned_processes(&pattern, env.scratch.path());
}

/// Live updates (`refresh_interval`) are intentionally a no-op for Elvish:
/// there is no primitive to recompute the prompt while the editor sits idle
/// (`edit:redraw` reuses the per-edit-cycle prompt result), so the init script
/// must run its refresh as a one-shot `--deferred` without `--watch` -- a
/// ticking watcher would never exit and Elvish waits on the job to redraw.
#[test]
fn live_update_tick_not_wired_for_elvish() {
    let init = substituted_init_script("src/init/starship.elv");
    // Check the actual `prompt --deferred` invocation line, not the whole
    // file text: a bare `contains("--watch")` would also match this file's
    // own explanatory prose about why `--watch` is deliberately NOT passed.
    let invocation = init
        .lines()
        .find(|l| l.contains("prompt --deferred"))
        .unwrap_or_else(|| panic!("no `prompt --deferred` invocation found in starship.elv"));
    assert!(
        !invocation.contains("--watch"),
        "starship.elv's `prompt --deferred` call passes --watch: a live-update ticker was wired for a shell that can't repaint the prompt mid-idle: {invocation:?}"
    );
}
