//! Automated regression tests for the Bash async-prompt integration in
//! `src/init/starship.bash`, exercised end to end against the real,
//! freshly built `starship` binary.
//!
//! None of these checks need a real terminal (`starship_precmd` never reads
//! from or draws to one), so -- like the original shell-script version of
//! this test -- everything here drives `bash -c "..."` as a plain piped
//! subprocess rather than through a pty.

use super::{
    CharacterConfig, PROCESS_COUNT_LOCK, STARSHIP_BIN, ScratchEnv, custom_module, real_bash_path,
    render_config, substituted_init_script,
};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the scratch config's slow `custom` module sleeps for. Comfortably
/// above `SLOW_MODULE_THRESHOLD` (5ms) but short enough that "kill mid-flight"
/// checks don't need to wait long, and long enough that "fast CacheRead"
/// checks have room to prove they're much quicker than this.
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
        fs::write(&init_script, substituted_init_script("src/init/starship.bash"))
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
            .map(|it| it.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_file()).collect())
            .unwrap_or_default()
    }

    /// Run `starship prompt [args]` directly (not through the init script),
    /// against this env's config/cache, with `STARSHIP_ASYNC` and
    /// `STARSHIP_SHELL=bash` set exactly like `starship_precmd` would export
    /// them -- the latter changes rendering (bash-specific `\[...\]`
    /// readline width-tracking escapes), so every direct call must see it or
    /// a byte-for-byte comparison against a script-driven render would
    /// spuriously mismatch.
    fn run_prompt(&self, config: &Path, async_enabled: bool, args: &[&str]) -> (String, Duration) {
        let start = Instant::now();
        let output = Command::new(STARSHIP_BIN.as_path())
            .arg("prompt")
            .args(args)
            .current_dir(self.path())
            .env("STARSHIP_CONFIG", config)
            .env("STARSHIP_CACHE", &self.scratch.cache_path)
            .env("STARSHIP_ASYNC", if async_enabled { "1" } else { "0" })
            .env("STARSHIP_SHELL", "bash")
            .output()
            .expect("failed to run starship prompt");
        (String::from_utf8_lossy(&output.stdout).into_owned(), start.elapsed())
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

    /// Like [`Self::run_script`], but spawned (not waited on) with piped
    /// stdout, for checks that need to interact with the process while it's
    /// still running (e.g. kill it mid-flight).
    fn spawn_script(&self, script: &str) -> Child {
        self.script_command(script, true)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn bash script")
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

    /// Assert no `starship prompt --async` process with a cwd inside this
    /// env's scratch dir is still running -- see
    /// [`super::assert_no_orphaned_processes`] for why cwd-scoping matters
    /// (this machine runs every other shell's test file concurrently too).
    fn assert_no_leaked_processes(&self) {
        let pattern = format!("{} prompt --async", STARSHIP_BIN.display());
        super::assert_no_orphaned_processes(&pattern, self.path());
    }

    fn leftover_tmp_files(&self) -> Vec<String> {
        let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let output = Command::new("find")
            .args([tmpdir.trim_end_matches('/'), "-maxdepth", "1", "-name", "starship-async.*", "-newer"])
            .arg(self.path())
            .output();
        output.map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()).unwrap_or_default()
    }

    fn clear_leftover_tmp_files(&self) {
        for f in self.leftover_tmp_files() {
            fs::remove_file(f).ok();
        }
    }
}

#[test]
fn cold_prompt_renders_slow_module_live() {
    let env = BashEnv::new();
    env.reset_cache();

    let (out, elapsed) = env.run_prompt(&env.scratch.config_path, true, &[]);
    assert!(out.contains("SLOW") && out.contains("FAST"), "output missing SLOW/FAST: {out}");
    assert!(elapsed.as_millis() >= 150, "expected a live (slow) render, took only {}ms", elapsed.as_millis());
}

#[test]
fn warm_cache_read_is_fast_and_shows_cached_value() {
    let env = BashEnv::new();
    env.reset_cache();
    env.run_prompt(&env.scratch.config_path, true, &["--async"]);

    let (out, elapsed) = env.run_prompt(&env.scratch.config_path, true, &[]);
    assert!(out.contains("SLOW"), "expected cached SLOW text, got: {out}");
    assert!(elapsed.as_millis() < 150, "expected a fast cached render, took {}ms", elapsed.as_millis());
}

/// Uses a scratch config whose slow module embeds a counter file, so we can
/// prove the cache entry's *content* actually changes between refreshes
/// (stronger, and more portable, than comparing filesystem mtimes across
/// platforms with differing `stat` flavors).
#[test]
fn async_refresh_populates_and_updates_cache() {
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
            Some(CharacterConfig { success_symbol: "[>](green)", ..Default::default() }),
        ),
    )
    .unwrap();

    assert!(env.cache_files().is_empty(), "expected an empty cache to start");
    env.run_prompt(&counter_config, true, &["--async"]);
    let cache_files = env.cache_files();
    assert_eq!(cache_files.len(), 1, "expected exactly one cache entry after one refresh");

    let content1 = fs::read_to_string(&cache_files[0]).unwrap_or_default();
    // Run again and confirm the cache entry's content is rewritten with the
    // new counter value, proving --async actually refreshes (not a one-shot
    // write that then goes stale).
    env.run_prompt(&counter_config, true, &["--async"]);
    let content2 = fs::read_to_string(&cache_files[0]).unwrap_or_default();

    assert!(content1.contains("SLOW-1"), "expected first refresh to write SLOW-1, got {content1:?}");
    assert!(content2.contains("SLOW-2"), "expected second refresh to write SLOW-2, got {content2:?}");
    assert_ne!(content1, content2, "cache entry content did not change between refreshes");
}

/// Drives the REAL `starship_precmd` function (via sourcing the init
/// script), not a hand-rolled mock, so it exercises `_starship_async_cancel`
/// exactly as a real interactive shell would: rapid repeated precmd cycles,
/// faster than the slow module's sleep, so refreshes are cancelled and
/// relaunched mid-flight repeatedly.
#[test]
fn no_orphans_after_rapid_precmd_cycles() {
    let env = BashEnv::new();
    env.reset_cache();
    env.clear_leftover_tmp_files();

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
    // Give any in-flight background refreshes a moment to finish or be reaped.
    std::thread::sleep(Duration::from_millis(500));

    env.assert_no_leaked_processes();
    assert!(env.leftover_tmp_files().is_empty(), "leftover tmp files: {:?}", env.leftover_tmp_files());
}

/// Kills the shell while a refresh is still in flight (immediately after
/// triggering a fresh precmd cycle, well before the slow module's sleep can
/// finish), exercising the EXIT trap exactly as a real interactive shell
/// would.
#[test]
fn no_orphans_after_kill_mid_refresh() {
    let env = BashEnv::new();
    env.reset_cache();
    env.clear_leftover_tmp_files();

    let _guard = PROCESS_COUNT_LOCK.lock().unwrap();
    // Launched directly with the real bash binary (not a wrapper subshell),
    // so the child pid unambiguously *is* the real Bash process that sources
    // init.bash and owns the EXIT trap.
    let mut child = env.spawn_script(
        r#"
source "$1"
export COLUMNS=80 SHLVL=1
starship_precmd
# Immediately signal readiness and hang; the parent will SIGTERM us while
# the background --async refresh is still running.
echo READY
sleep 5
"#,
    );

    // Wait for the child to signal it has started the refresh ("READY"),
    // then kill it fast.
    let mut stdout = child.stdout.take().unwrap();
    let mut acc = String::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !acc.contains("READY") {
        match stdout.read(&mut byte) {
            Ok(1) => acc.push(byte[0] as char),
            _ => break,
        }
    }
    let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(500));

    env.assert_no_leaked_processes();
    assert!(env.leftover_tmp_files().is_empty(), "leftover tmp files: {:?}", env.leftover_tmp_files());
}

/// STARSHIP_ASYNC=0 must reproduce the old fully synchronous behavior
/// byte-for-byte and install none of the async machinery.
#[test]
fn disabled_async_matches_direct_render_byte_for_byte() {
    let env = BashEnv::new();
    env.reset_cache();

    let (direct_expected, _) = env.run_prompt(
        &env.scratch.config_path,
        true,
        &["--terminal-width=80", "--status=0", "--pipestatus=0", "--jobs=0", "--shlvl=1"],
    );

    // PS1 itself may legitimately contain newlines (multi-line prompt
    // formats), so we can't split the script's stdout on lines to recover
    // it. Instead, bracket it between unique markers and extract everything
    // in between.
    let out = env.run_script(
        r#"
source "$1"
export COLUMNS=80 SHLVL=1
starship_precmd
printf '===PS1_START===%s===PS1_END===\n' "$PS1"
"#,
        false,
    );
    let sync_ps1 = out
        .split_once("===PS1_START===")
        .and_then(|(_, rest)| rest.split_once("===PS1_END==="))
        .map(|(ps1, _)| ps1.to_string())
        .unwrap_or_default();

    assert_eq!(sync_ps1, direct_expected, "STARSHIP_ASYNC=0 output should be byte-identical to a direct `prompt` render");
}

#[test]
fn disabled_async_installs_no_helpers() {
    let env = BashEnv::new();
    env.reset_cache();

    let out = env.run_script(
        r#"
source "$1"
echo "GETTER_TYPE=$(type -t _starship_get_prompt 2>/dev/null)"
echo "CANCEL_TYPE=$(type -t _starship_async_cancel 2>/dev/null)"
"#,
        false,
    );
    let getter_type = out.lines().find_map(|l| l.strip_prefix("GETTER_TYPE=")).unwrap_or_default();
    let cancel_type = out.lines().find_map(|l| l.strip_prefix("CANCEL_TYPE=")).unwrap_or_default();

    assert!(getter_type.is_empty(), "STARSHIP_ASYNC=0 should not define _starship_get_prompt, got {getter_type:?}");
    assert!(cancel_type.is_empty(), "STARSHIP_ASYNC=0 should not define _starship_async_cancel, got {cancel_type:?}");
}

#[test]
fn disabled_async_installs_no_exit_trap() {
    let env = BashEnv::new();
    env.reset_cache();

    let out = env.run_script(
        r#"
source "$1"
echo "EXIT_TRAP=$(trap -p EXIT)"
"#,
        false,
    );
    let exit_trap_line = out.lines().find(|l| l.starts_with("EXIT_TRAP=")).unwrap_or_default();

    assert_eq!(exit_trap_line, "EXIT_TRAP=", "STARSHIP_ASYNC=0 should install no EXIT trap");
}
