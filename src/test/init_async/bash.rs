//! Automated regression tests for the Bash async-prompt integration in
//! `src/init/starship.bash`, exercised end to end against the real,
//! freshly built `starship` binary.
//!
//! Bash's integration is the simplest expression of the async model: the
//! precmd paints with `--cached` and fires one fire-and-forget
//! `starship prompt --deferred` (no `--watch`, no cancellation, no repaint --
//! readline can't re-expand PS1 mid-line, so refreshed values appear on the
//! next prompt draw).
//!
//! None of these checks need a real terminal (`starship_precmd` never reads
//! from or draws to one), so everything here drives `bash -c "..."` as a
//! plain piped subprocess rather than through a pty.

use super::{
    custom_module, real_bash_path, render_config, substituted_init_script, CharacterConfig,
    ScratchEnv, PROCESS_COUNT_LOCK, STARSHIP_BIN,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the scratch config's slow `custom` module sleeps for. Comfortably
/// above `SLOW_MODULE_THRESHOLD` (5ms) but short enough that waiting for
/// in-flight refreshes to settle is cheap, and long enough that "fast cached
/// paint" checks have room to prove they're much quicker than this.
const SLOW_MODULE_MS: u64 = 200;

/// A scratch Bash environment: an isolated `STARSHIP_CONFIG`/`STARSHIP_CACHE`
/// (see [`ScratchEnv`]), a real (non-shim) bash binary, and the real
/// `starship.bash` init script with `::STARSHIP::` substituted for the real
/// built binary -- everything each check needs, built once per test.
struct BashEnv {
    scratch: ScratchEnv,
    bash: PathBuf,
    init_script: PathBuf,
}

impl BashEnv {
    fn new() -> Self {
        let scratch = ScratchEnv::new(SLOW_MODULE_MS).expect("failed to set up scratch env");
        let init_script = scratch.path().join("init.bash");
        fs::write(
            &init_script,
            substituted_init_script("src/init/starship.bash"),
        )
        .expect("failed to write init.bash");
        Self {
            scratch,
            bash: real_bash_path().into(),
            init_script,
        }
    }

    fn path(&self) -> &Path {
        self.scratch.path()
    }

    fn reset_cache(&self) {
        fs::remove_dir_all(&self.scratch.cache_path).ok();
        fs::create_dir_all(&self.scratch.cache_path).unwrap();
    }

    fn cache_files(&self) -> Vec<PathBuf> {
        let dir = self.scratch.cache_path.join("cache");
        fs::read_dir(&dir)
            .map(|it| {
                it.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Run `starship prompt [args]` directly (not through the init script),
    /// against this env's config/cache, with `STARSHIP_SHELL=bash` set exactly
    /// like `starship_precmd` would export it -- that changes rendering
    /// (bash-specific `\[...\]` readline width-tracking escapes), so every
    /// direct call must see it or a byte-for-byte comparison against a
    /// script-driven render would spuriously mismatch.
    fn run_prompt(&self, config: &Path, args: &[&str]) -> (String, Duration) {
        let start = Instant::now();
        let output = Command::new(STARSHIP_BIN.as_path())
            .arg("prompt")
            .args(args)
            .current_dir(self.path())
            .env("STARSHIP_CONFIG", config)
            .env("STARSHIP_CACHE", &self.scratch.cache_path)
            .env("STARSHIP_SHELL", "bash")
            .output()
            .expect("failed to run starship prompt");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            start.elapsed(),
        )
    }

    /// Run `script` (sourcing this env's init script as `$1`) to completion
    /// via a real bash process and return its captured stdout.
    fn run_script(&self, script: &str, async_enabled: bool) -> String {
        let output = self
            .script_command(script, async_enabled)
            .output()
            .expect("failed to run bash script");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn script_command(&self, script: &str, async_enabled: bool) -> Command {
        let mut cmd = Command::new(&self.bash);
        cmd.args(["--noprofile", "--norc", "-c", script, "_"])
            .arg(&self.init_script)
            .current_dir(self.path())
            .env("STARSHIP_CONFIG", &self.scratch.config_path)
            .env("STARSHIP_CACHE", &self.scratch.cache_path)
            .env("STARSHIP_ASYNC", if async_enabled { "1" } else { "0" });
        cmd
    }

    /// Assert no `starship prompt --deferred` process with a cwd inside this
    /// env's scratch dir is still running -- see
    /// [`super::assert_no_orphaned_processes`] for why cwd-scoping matters
    /// (this machine runs every other shell's test file concurrently too).
    fn assert_no_leaked_processes(&self) {
        let pattern = format!("{} prompt --deferred", STARSHIP_BIN.display());
        super::assert_no_orphaned_processes(&pattern, self.path());
    }
}

/// Extract PS1 from a script's stdout, bracketed between unique markers --
/// PS1 itself may legitimately contain newlines (multi-line prompt formats),
/// so splitting the output on lines can't recover it.
fn extract_ps1(out: &str) -> String {
    out.split_once("===PS1_START===")
        .and_then(|(_, rest)| rest.split_once("===PS1_END==="))
        .map(|(ps1, _)| ps1.to_string())
        .unwrap_or_default()
}

#[test]
fn cold_cached_paint_renders_slow_module_live() {
    let env = BashEnv::new();
    env.reset_cache();

    // With nothing recorded yet, --cached falls through and computes the slow
    // module live -- correct output, just not fast yet.
    let (out, elapsed) = env.run_prompt(&env.scratch.config_path, &["--cached"]);
    assert!(
        out.contains("SLOW") && out.contains("FAST"),
        "output missing SLOW/FAST: {out}"
    );
    assert!(
        elapsed.as_millis() >= 150,
        "expected a live (slow) render, took only {}ms",
        elapsed.as_millis()
    );
}

#[test]
fn warm_cached_paint_is_fast_and_shows_recorded_value() {
    let env = BashEnv::new();
    env.reset_cache();
    env.run_prompt(&env.scratch.config_path, &["--deferred"]);

    let (out, elapsed) = env.run_prompt(&env.scratch.config_path, &["--cached"]);
    assert!(
        out.contains("SLOW"),
        "expected cached SLOW text, got: {out}"
    );
    assert!(
        elapsed.as_millis() < 150,
        "expected a fast cached render, took {}ms",
        elapsed.as_millis()
    );
}

/// Uses a scratch config whose slow module embeds a counter file, so we can
/// prove the snapshot's *content* actually changes between refreshes
/// (stronger, and more portable, than comparing filesystem mtimes across
/// platforms with differing `stat` flavors).
#[test]
fn deferred_refresh_populates_and_updates_cache() {
    let env = BashEnv::new();
    env.reset_cache();

    let counter_file = env.path().join("counter.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_config = env.path().join("starship-counter.toml");
    fs::write(
        &counter_config,
        render_config(
            "${custom.slow}$character",
            None,
            true,
            &[(
                "slow",
                custom_module(&format!(
                    "sleep 0.2 && n=$(cat '{0}'); n=$((n + 1)); echo $n > '{0}'; echo SLOW-$n",
                    counter_file.display()
                )),
            )],
            Some(CharacterConfig {
                success_symbol: "[>](green)",
                ..Default::default()
            }),
        ),
    )
    .unwrap();

    assert!(
        env.cache_files().is_empty(),
        "expected an empty cache to start"
    );
    env.run_prompt(&counter_config, &["--deferred"]);
    let cache_files = env.cache_files();
    assert_eq!(
        cache_files.len(),
        1,
        "expected exactly one snapshot after one refresh (one file per directory)"
    );

    let content1 = fs::read_to_string(&cache_files[0]).unwrap_or_default();
    // Run again and confirm the snapshot is rewritten with the new counter
    // value, proving --deferred actually refreshes (not a one-shot write that
    // then goes stale).
    env.run_prompt(&counter_config, &["--deferred"]);
    let content2 = fs::read_to_string(&cache_files[0]).unwrap_or_default();

    assert!(
        content1.contains("SLOW-1"),
        "expected first refresh to write SLOW-1, got {content1:?}"
    );
    assert!(
        content2.contains("SLOW-2"),
        "expected second refresh to write SLOW-2, got {content2:?}"
    );
    assert_ne!(
        content1, content2,
        "snapshot content did not change between refreshes"
    );
}

/// Drives the REAL `starship_precmd` function (via sourcing the init script),
/// not a hand-rolled mock: the precmd's fire-and-forget refresh must populate
/// the cache so the *next* precmd's --cached paint shows the slow module
/// -- the whole Bash async story end to end.
#[test]
fn precmd_refresh_lands_on_next_prompt_draw() {
    let env = BashEnv::new();
    env.reset_cache();

    let out = env.run_script(
        r#"
source "$1"
export COLUMNS=80 SHLVL=1
starship_precmd
# Give the fire-and-forget --deferred refresh time to finish...
sleep 1
# ...then confirm the next draw serves the recorded value.
starship_precmd
printf '===PS1_START===%s===PS1_END===\n' "$PS1"
"#,
        true,
    );

    let ps1 = extract_ps1(&out);
    assert!(
        ps1.contains("SLOW"),
        "second precmd's PS1 should contain the refreshed slow module, got {ps1:?}"
    );
    assert!(
        !env.cache_files().is_empty(),
        "precmd's background refresh never wrote the cache snapshot"
    );
}

/// Rapid precmd cycles fire overlapping fire-and-forget refreshes; each is
/// bounded (it renders once and exits), so none may linger once they've had
/// time to finish. There is deliberately no cancellation to test: overlapping
/// refreshes are safe (atomic snapshot replace; newest wins).
#[test]
fn no_orphans_after_rapid_precmd_cycles() {
    let env = BashEnv::new();
    env.reset_cache();

    let _guard = PROCESS_COUNT_LOCK.lock().unwrap();
    env.run_script(
        r#"
source "$1"
export COLUMNS=80 SHLVL=1
for i in $(seq 1 15); do
    starship_precmd
done
"#,
        true,
    );
    // Every refresh is a one-shot render (slow module: 200ms); give the last
    // wave time to finish on its own.
    std::thread::sleep(Duration::from_millis(1500));

    env.assert_no_leaked_processes();
}

/// Killing the shell mid-refresh orphans the in-flight one-shot refresh; it
/// must finish its bounded render and exit on its own -- no EXIT-trap
/// machinery exists (or is needed) to reap it.
#[test]
fn orphaned_refresh_exits_on_its_own_after_shell_death() {
    let env = BashEnv::new();
    env.reset_cache();

    let _guard = PROCESS_COUNT_LOCK.lock().unwrap();
    let mut child = env
        .script_command(
            r#"
source "$1"
export COLUMNS=80 SHLVL=1
starship_precmd
"#,
            true,
        )
        .spawn()
        .expect("failed to spawn bash script");
    // Kill the shell fast, while the 200ms slow module is still rendering.
    std::thread::sleep(Duration::from_millis(50));
    let _ = Command::new("kill")
        .args(["-9", &child.id().to_string()])
        .status();
    let _ = child.wait();

    std::thread::sleep(Duration::from_millis(1000));
    env.assert_no_leaked_processes();
}

/// STARSHIP_ASYNC=0 must reproduce the old fully synchronous behavior
/// byte-for-byte: a plain (Direct) render, no --cached flag, no cache writes.
#[test]
fn disabled_async_matches_direct_render_and_never_touches_cache() {
    let env = BashEnv::new();
    env.reset_cache();

    let (direct_expected, _) = env.run_prompt(
        &env.scratch.config_path,
        &[
            "--terminal-width=80",
            "--status=0",
            "--pipestatus=0",
            "--jobs=0",
            "--shlvl=1",
        ],
    );

    let out = env.run_script(
        r#"
source "$1"
export COLUMNS=80 SHLVL=1
starship_precmd
printf '===PS1_START===%s===PS1_END===\n' "$PS1"
"#,
        false,
    );
    let sync_ps1 = extract_ps1(&out);

    assert_eq!(
        sync_ps1, direct_expected,
        "STARSHIP_ASYNC=0 output should be byte-identical to a direct `prompt` render"
    );

    // No background refresh may have fired: Direct mode never writes cache.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        env.cache_files().is_empty(),
        "STARSHIP_ASYNC=0 wrote cache entries: a --deferred refresh was fired"
    );
}

/// Live updates (`refresh_interval`) are intentionally a no-op for Bash:
/// readline won't re-expand PS1 mid-line, so a periodic timer cannot advance a
/// live module while the user idles. The init script must therefore fire
/// `--deferred` without `--watch`.
#[test]
fn live_update_tick_not_wired_for_bash() {
    let init = substituted_init_script("src/init/starship.bash");
    assert!(
        !init.contains("--watch"),
        "starship.bash passes --watch: a live-update ticker was wired for a shell that can't repaint the prompt mid-idle"
    );
}
