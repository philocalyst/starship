//! Automated, self-contained regression tests for the async prompt
//! integration in `src/init/starship.ps1`. Drives a real pwsh -- both
//! non-interactively (`-File`) and interactively over a real pty via the
//! shared harness -- against the real built starship binary.
//!
//! The PowerShell model under test is deliberately minimal: the prompt
//! function paints with `--cached` and fires one fire-and-forget
//! `starship prompt --deferred` via `Invoke-Native -NoWait` (a bare
//! `System.Diagnostics.Process`, no PowerShell jobs, no OnIdle event
//! machinery -- both were confirmed unable to repaint in place on this
//! platform, so the refreshed values simply appear on the next prompt
//! draw). The checks therefore assert: warm `--cached` paints are fast and
//! show the recorded value, the fire-and-forget refresh populates the
//! cache, nothing ever lands in `Get-Job`/`Get-EventSubscriber`, and
//! `STARSHIP_ASYNC=0` reproduces the classic synchronous path.
//!
//! Locates `pwsh` on `$PATH`, falling back to the known preview-cask
//! install location if absent.

use super::{
    CharacterConfig, CustomConfig, PtySession, STARSHIP_BIN, SpawnOptions, StarshipRootConfig,
    substituted_init_script,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

/// Locate a usable `pwsh` binary: prefer `$PATH`, then the known Homebrew
/// preview-cask install location (this environment may not have `pwsh` on
/// `$PATH` even when installed -- see module docs in the source test this
/// was ported from).
static PWSH_BIN: LazyLock<PathBuf> = LazyLock::new(|| {
    for candidate in ["pwsh", "pwsh-preview"] {
        if let Ok(out) = Command::new("command").args(["-v", candidate]).output() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }
    for candidate in [
        "/usr/local/microsoft/powershell/7-preview/pwsh",
        "/usr/local/bin/pwsh",
        "/usr/local/bin/pwsh-preview",
        "/opt/homebrew/bin/pwsh",
        "/opt/homebrew/bin/pwsh-preview",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "pwsh not found on PATH or at any known install location; install with \
         `brew install --cask powershell` (or `powershell@preview`)"
    );
});

/// A scratch `starship.toml`'s top-level shape for these tests, composed
/// entirely from starship's own real config structs (`StarshipRootConfig`,
/// `CharacterConfig`, `CustomConfig`) via their own `Serialize` impls -- not
/// a hand-built, stringly-keyed TOML string -- so a field getting renamed in
/// the real schema is a compile error here, not a silently-ignored/misparsed
/// key in a generated file that happens to still parse. (A previous
/// hand-built `toml::Value::Table` approach had a real bug that produced
/// TOML starship silently failed to parse.)
#[derive(serde::Serialize)]
struct Ps1Config<'a> {
    #[serde(flatten)]
    root: StarshipRootConfig,
    character: CharacterConfig<'a>,
    custom: indexmap::IndexMap<&'a str, CustomConfig<'a>>,
}

/// Build the scratch `starship.toml` shared by every check in this file: a
/// fast module and a module exceeding `SLOW_MODULE_THRESHOLD` (5ms) that
/// emits a unique marker per render, so checks can tell "cold/live compute"
/// apart from "served from cache" apart from "recomputed by the background
/// refresh". `command_timeout` is raised well above the slow module's sleep
/// so starship itself never kills it mid-flight.
fn render_ps1_config() -> String {
    let mut root = StarshipRootConfig::default();
    root.format = "$custom$character".to_string();
    root.add_newline = false;
    root.command_timeout = 5000;

    let character = CharacterConfig {
        success_symbol: "[>](bold green)",
        ..Default::default()
    };

    // The custom module commands run through starship's own default shell
    // resolution (no explicit `shell =`, matching the original hand-written
    // config), which on this platform resolves to a real `sh`/bash-compatible
    // shell -- so bash syntax (`sleep 0.3`, `$(date +%s%N)`) is fine here;
    // this is independent of pwsh, since these are starship *custom module*
    // commands, not pwsh commands.
    let mut custom = indexmap::IndexMap::new();
    custom.insert(
        "fast",
        CustomConfig {
            command: "echo FAST",
            when: crate::config::Either::First(true),
            format: "[$output]($style) ",
            style: "green",
            ..Default::default()
        },
    );
    custom.insert(
        "slow",
        CustomConfig {
            command: "sleep 0.3 && echo SLOW-$(date +%s%N)",
            when: crate::config::Either::First(true),
            format: "[$output]($style) ",
            style: "yellow",
            ..Default::default()
        },
    );

    let scratch = Ps1Config { root, character, custom };
    toml::to_string(&scratch).expect("failed to serialize scratch ps1 config")
}

/// Pull `KEY=value` out of `KEY=value` lines emitted via `Write-Host`,
/// mirroring the original bash harness's `grep -oE 'KEY=...' | cut -d= -f2`
/// extraction.
fn extract_var<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    text.lines().find_map(|l| l.strip_prefix(prefix.as_str())).map(str::trim)
}

fn count_files_recursive(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            count += count_files_recursive(&path);
        } else if path.is_file() {
            count += 1;
        }
    }
    count
}

/// A scratch PowerShell environment: an isolated scratch dir with a real
/// `starship.toml` (fast + slow custom modules, see [`render_ps1_config`]),
/// an isolated `STARSHIP_CACHE`, a real `pwsh` binary location, and the real
/// `starship.ps1` init script with `::STARSHIP::` substituted for the real
/// built binary -- everything each check needs, built once per test.
///
/// Mirrors `BashEnv` in `bash.rs`; the main difference is that ps1 checks
/// split across two execution modes depending on whether they need genuine
/// PSReadLine/OnIdle interactivity:
///   * [`Self::run_file`] drives a `.ps1` script non-interactively via
///     `pwsh -NoProfile -File`, for checks that just need `prompt` called
///     directly (timing, leak-counting, the ASYNC=0 check).
///   * [`Self::spawn_interactive`] drives a real interactive pwsh over a
///     real pty via [`PtySession`], for checks that need genuine
///     interactive behavior (the cache-population check).
struct Ps1Env {
    root: tempfile::TempDir,
    config_path: PathBuf,
    init_path: PathBuf,
}

impl Ps1Env {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("failed to create scratch workdir");

        let config_path = root.path().join("starship.toml");
        fs::write(&config_path, render_ps1_config()).unwrap();

        let init_content = substituted_init_script("src/init/starship.ps1");
        let init_path = root.path().join("starship_init.ps1");
        fs::write(&init_path, &init_content).unwrap();
        assert!(
            init_content.contains(&STARSHIP_BIN.display().to_string()),
            "::STARSHIP:: substitution failed"
        );
        assert!(!init_content.contains("::STARSHIP::"), "leftover ::STARSHIP:: token after substitution");

        Self {
            root,
            config_path,
            init_path,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    /// A fresh, empty cache dir under this env's scratch root, named `name`
    /// so parallel checks that want their own isolated cache within the same
    /// env don't collide.
    fn fresh_cache(&self, name: &str) -> PathBuf {
        let cache = self.path().join(name);
        fs::create_dir_all(&cache).unwrap();
        cache
    }

    /// Write `body` as a `.ps1` script under the scratch root and run it
    /// non-interactively via `pwsh -NoProfile -File`, returning captured
    /// stdout+stderr. `body` can reference `{config}`, `{cache}`, `{root}`,
    /// and `{init}` as format-string placeholders for this env's paths.
    fn run_file(&self, name: &str, cache: &Path, body: &str, timeout: Duration) -> String {
        let script_path = self.path().join(name);
        let filled = body
            .replace("{config}", &self.config_path.display().to_string())
            .replace("{cache}", &cache.display().to_string())
            .replace("{root}", &self.path().display().to_string())
            .replace("{init}", &self.init_path.display().to_string());
        fs::write(&script_path, filled).unwrap();
        run_ps1_file(&script_path, timeout)
    }

    /// Spawn a real interactive pwsh over a real pty against `cache`, with
    /// `STARSHIP_CONFIG`/`STARSHIP_CACHE`/`STARSHIP_ASYNC=1` set on the child
    /// process itself (mirroring the original Python pty driver, which
    /// passed these via `os.environ.update(env_extra)` before `exec`, not by
    /// typing `$env:X = 'Y'` into the shell after startup), then sources
    /// `init_path` and presses Enter to trigger the first prompt draw.
    fn spawn_interactive(&self, cache: &Path, init_path: &Path) -> PtySession {
        let cache_str = cache.display().to_string();
        let config_str = self.config_path.display().to_string();
        let mut session = PtySession::spawn(SpawnOptions {
            program: PWSH_BIN.to_str().expect("non-utf8 pwsh path"),
            args: &["-NoProfile", "-NoLogo"],
            envs: &[
                ("STARSHIP_CONFIG", config_str.as_str()),
                ("STARSHIP_CACHE", cache_str.as_str()),
                ("STARSHIP_ASYNC", "1"),
            ],
            cwd: Some(self.path()),
        })
        .expect("failed to spawn pwsh");
        // Let the shell finish starting up (module loads, PSReadLine init)
        // before typing anything, exactly like the original Python pty
        // driver's first {"send": null, "wait": 1.5} step.
        session.pump(Duration::from_millis(1500));
        session.send_and_pump(&format!(". '{}'\r", init_path.display()), Duration::from_millis(2000));
        session
    }
}

/// Run a `.ps1` script file non-interactively (`pwsh -NoProfile -File`,
/// mirroring the original bash harness's use of `-File` mode for checks that
/// don't need live interactivity -- e.g. the leak-check loop just calls
/// `prompt` directly N times) and return its captured stdout+stderr.
fn run_ps1_file(script_path: &Path, timeout: Duration) -> String {
    // `timeout` isn't guaranteed present on macOS by default; enforce the
    // budget from the Rust side instead via a watchdog thread-free approach:
    // spawn and wait with an explicit deadline using a child-killing helper.
    let mut child = Command::new(PWSH_BIN.as_path())
        .args(["-NoProfile", "-File"])
        .arg(script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn pwsh -File");

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    let output = child.wait_with_output().expect("failed to collect pwsh output");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// CHECK 1: warm CacheRead `prompt` call returns fast (well under the slow
/// module's 300ms sleep) and still contains the slow module's cached output.
#[test]
fn check1_warm_cache_read_is_fast_and_has_slow_output() {
    let env = Ps1Env::new();
    let cache = env.fresh_cache("cache_timing");

    let log = env.run_file(
        "timing_check.ps1",
        &cache,
        r#"$env:STARSHIP_CONFIG = '{config}'
$env:STARSHIP_CACHE = '{cache}'
$env:STARSHIP_ASYNC = '1'
Set-Location '{root}'
Import-Module PSReadLine -ErrorAction SilentlyContinue
. '{init}'
# Prime: first call computes the slow module live (cold cache) and starts
# the background refresh. Wait for it to land so the SECOND call is a true
# CacheRead hit, then time that second call: it must return in well under
# the module's 300ms sleep, proving it read from cache rather than
# recomputing live.
$null = prompt
Start-Sleep -Milliseconds 900
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$r = prompt
$ms = $sw.ElapsedMilliseconds
Write-Host "TIMING_MS=$ms"
Write-Host "TIMING_HAS_SLOW=$($r -match 'SLOW-')"
"#,
        Duration::from_secs(30),
    );

    let timing_ms = extract_var(&log, "TIMING_MS").and_then(|v| v.parse::<u64>().ok());
    let timing_has_slow = extract_var(&log, "TIMING_HAS_SLOW");

    assert!(
        timing_ms.is_some_and(|ms| ms < 150),
        "check1a: warm CacheRead prompt() call should return fast: TIMING_MS={timing_ms:?}, expected <150ms; log: {log}"
    );
    assert!(
        timing_has_slow == Some("True"),
        "check1b: fast prompt() call should still contain the slow module's cached output: \
         TIMING_HAS_SLOW={timing_has_slow:?}; log: {log}"
    );
}

/// CHECK 2: a real interactive prompt draw fires the fire-and-forget
/// `--deferred` refresh, which populates the on-disk cache. Needs a real
/// interactive session (not just `-File`), since it's driven by the real
/// `global:prompt` function PowerShell itself invokes on each draw.
#[test]
fn check2_background_refresh_populates_cache() {
    let env = Ps1Env::new();
    let cache = env.fresh_cache("cache_populate");

    let mut session = env.spawn_interactive(&cache, &env.init_path);
    session.send_and_pump("\r", Duration::from_millis(1500));

    let cache_files_after = count_files_recursive(&cache);
    assert!(
        cache_files_after > 0,
        "check2: the fire-and-forget --deferred refresh should populate the on-disk cache; found \
         {cache_files_after} file(s) under {}",
        cache.display()
    );
}

/// CHECK 3: the model deliberately uses no PowerShell job or event-
/// subscription machinery -- `Invoke-Native -NoWait` is a bare
/// `System.Diagnostics.Process`, so `Get-Job`/`Get-EventSubscriber` must stay
/// at zero even after several prompt draws (each firing its own
/// fire-and-forget refresh).
#[test]
fn check3_no_job_or_event_subscription_machinery() {
    let env = Ps1Env::new();
    let cache = env.fresh_cache("cache_no_jobs");

    let log = env.run_file(
        "no_jobs_check.ps1",
        &cache,
        r#"$env:STARSHIP_CONFIG = '{config}'
$env:STARSHIP_CACHE = '{cache}'
$env:STARSHIP_ASYNC = '1'
Set-Location '{root}'
Import-Module PSReadLine -ErrorAction SilentlyContinue
. '{init}'

for ($i = 0; $i -lt 5; $i++) {{
    $null = prompt
    Start-Sleep -Milliseconds 100
}}
$jc = (Get-Job -ErrorAction SilentlyContinue | Measure-Object).Count
$sc = (Get-EventSubscriber -Force -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "JOB_COUNT=$jc"
Write-Host "SUB_COUNT=$sc"
"#,
        Duration::from_secs(30),
    );

    let job_count = extract_var(&log, "JOB_COUNT");
    let sub_count = extract_var(&log, "SUB_COUNT");
    assert_eq!(
        job_count,
        Some("0"),
        "check3a: no PowerShell job should ever be created; log: {log}"
    );
    assert_eq!(
        sub_count,
        Some("0"),
        "check3b: no event subscription should ever be created; log: {log}"
    );
}

/// CHECK 4: rapid prompt cycles fire overlapping fire-and-forget refreshes;
/// each is a bounded one-shot render (the slow module's 0.3s sleep) and
/// process-managed by the OS, not by starship.ps1 -- none may still be
/// running once they've had time to finish.
#[test]
fn check4_no_orphaned_refresh_processes_after_rapid_cycles() {
    let env = Ps1Env::new();
    let cache = env.fresh_cache("cache_leak");

    env.run_file(
        "leak_check.ps1",
        &cache,
        r#"$env:STARSHIP_CONFIG = '{config}'
$env:STARSHIP_CACHE = '{cache}'
$env:STARSHIP_ASYNC = '1'
Set-Location '{root}'
Import-Module PSReadLine -ErrorAction SilentlyContinue
. '{init}'

for ($i = 0; $i -lt 8; $i++) {{
    $null = prompt
}}
Start-Sleep -Milliseconds 1500
Write-Host DONE
"#,
        Duration::from_secs(40),
    );

    let pattern = format!("{} prompt --deferred", STARSHIP_BIN.display());
    super::assert_no_orphaned_processes(&pattern, env.path());
}

/// CHECK 5: `STARSHIP_ASYNC=0` skips the fire-and-forget refresh entirely,
/// never touches the cache, and matches the old fully synchronous behavior.
#[test]
fn check5_async_disabled_skips_refresh_and_cache() {
    let env = Ps1Env::new();
    let cache = env.fresh_cache("cache_disabled");

    let log = env.run_file(
        "disabled_check.ps1",
        &cache,
        r#"$env:STARSHIP_CONFIG = '{config}'
$env:STARSHIP_CACHE = '{cache}'
$env:STARSHIP_ASYNC = '0'
Set-Location '{root}'
Import-Module PSReadLine -ErrorAction SilentlyContinue
. '{init}'

$r = prompt
Start-Sleep -Milliseconds 500
$jobCount = (Get-Job -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "DISABLED_HAS_SLOW=$($r -match 'SLOW-')"
Write-Host "DISABLED_JOB_COUNT=$jobCount"
"#,
        Duration::from_secs(30),
    );

    let disabled_has_slow = extract_var(&log, "DISABLED_HAS_SLOW");
    let disabled_job_count = extract_var(&log, "DISABLED_JOB_COUNT");

    assert!(
        disabled_job_count == Some("0"),
        "check5a: STARSHIP_ASYNC=0 should never create a background job: \
         DISABLED_JOB_COUNT={disabled_job_count:?}; log: {log}"
    );
    assert!(
        disabled_has_slow == Some("True"),
        "check5b: STARSHIP_ASYNC=0 should still render the slow module synchronously: \
         DISABLED_HAS_SLOW={disabled_has_slow:?}; log: {log}"
    );
    assert_eq!(
        count_files_recursive(&cache),
        0,
        "check5c: STARSHIP_ASYNC=0 should never write to the cache (no --deferred fired)"
    );
}

/// Live updates (`refresh_interval`) are intentionally a no-op for PowerShell:
/// there is no thread-safe way to drive PSReadLine's InvokePrompt from a
/// periodic timer, so the fire-and-forget refresh must run without `--watch`.
#[test]
fn live_update_tick_not_wired_for_powershell() {
    let init = substituted_init_script("src/init/starship.ps1");
    // Check the actual `Invoke-Native ... -Arguments (... + "--deferred")`
    // invocation line, not the whole file text: a bare `contains("--watch")`
    // would also match this file's own explanatory prose about why
    // `--watch` is deliberately NOT passed.
    let invocation = init
        .lines()
        .find(|l| l.contains("\"--deferred\""))
        .unwrap_or_else(|| panic!("no `--deferred` invocation found in starship.ps1"));
    assert!(
        !invocation.contains("--watch"),
        "starship.ps1's --deferred invocation passes --watch: a live-update ticker was wired for a shell that can't repaint the prompt mid-idle: {invocation:?}"
    );
}
