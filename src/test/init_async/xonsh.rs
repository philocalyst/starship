//! Automated regression tests for the xonsh async-prompt integration in
//! `src/init/starship.xsh`. Drives a real xonsh (`--shell-type=prompt_toolkit`)
//! session over a real pty against the real built starship binary, and
//! asserts the async-prompt contract end to end:
//!
//!   1. The very first prompt paints synchronously and is not empty.
//!   2. Once the cache is warm, the `on_pre_prompt` background worker
//!      refreshes `_STARSHIP_LEFT`/`_STARSHIP_RIGHT` via `--async` and
//!      `app.invalidate()` fires, and the content actually changes (proven
//!      via a unique per-render marker).
//!   3. `on_pre_prompt` fires once per logical prompt line -- NOT once per
//!      resize.
//!   4. `STARSHIP_ASYNC=0` fully disables the async machinery: no hook
//!      registered, no background thread spawned, and render time matches
//!      the old fully-synchronous single-call path. This guards a real bug
//!      that was found and fixed: `$STARSHIP_ASYNC` used to be hardcoded to
//!      `'1'` with no opt-out path; this test verifies the fix actually
//!      disables everything, not just that the variable happens to read
//!      `'0'`.
//!   5. Abruptly killing the session mid-refresh leaves no zombie `starship`
//!      processes and no corrupted cache (no stray non-tmp partial writes).
//!
//! Like the original Python-pty-based shell-script test this replaces, each
//! check sources the real init script with `::STARSHIP::` substituted for
//! the real binary, then types Python expressions directly into the xonsh
//! REPL over the pty to introspect internal interpreter state (e.g.
//! `_STARSHIP_LEFT`, `__xonsh__.shell.shell.prompter.app`). Those
//! expressions execute *inside xonsh's own Python interpreter*, not on the
//! Rust/test-driver side -- the driver here only ever writes bytes to a pty
//! and reads them back through a real terminal emulator, exactly like
//! `bash.rs`/`zsh.rs`. No Python runs anywhere in the test driver itself.

use super::{
    CharacterConfig, CustomConfig, PtySession, STARSHIP_BIN, SpawnOptions, StarshipRootConfig,
    custom_module, extract_tag, json_field, substituted_init_script,
};
use crate::configs::time::TimeConfig;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

/// Locate a working xonsh binary. `xonsh` may or may not be on `$PATH`; a
/// common `pip3 install --user xonsh` location on macOS with Xcode's
/// python3 is `~/Library/Python/<version>/bin/xonsh`. If truly missing,
/// install it (small, reversible, user-local install) exactly like the
/// original shell-script harness did.
static XONSH_BIN: LazyLock<PathBuf> = LazyLock::new(find_or_install_xonsh);

fn find_xonsh() -> Option<PathBuf> {
    if let Ok(out) = Command::new("command").args(["-v", "xonsh"]).output() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() && Path::new(&path).is_file() {
            return Some(PathBuf::from(path));
        }
    }
    let home = std::env::var("HOME").ok()?;
    for version in ["3.9", "3.10", "3.11", "3.12", "3.13"] {
        let candidate = PathBuf::from(&home)
            .join("Library/Python")
            .join(version)
            .join("bin/xonsh");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let candidate = PathBuf::from(&home).join(".local/bin/xonsh");
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

fn find_or_install_xonsh() -> PathBuf {
    if let Some(bin) = find_xonsh() {
        return bin;
    }
    eprintln!("== xonsh not found, installing via pip3 --user...");
    let status = Command::new("pip3")
        .args(["install", "--user", "xonsh", "prompt_toolkit"])
        .status()
        .expect("failed to run pip3 install");
    assert!(status.success(), "pip3 install xonsh failed");
    find_xonsh().expect("could not locate xonsh binary after install attempt")
}

/// xonsh's shebang interpreter resolves user-site-packages relative to
/// `$HOME` at *runtime*, not at install time. Since each check below
/// sandboxes `HOME` for isolation (a fresh scratch dir per session, mirroring
/// the real user's dotfile-free environment), the interpreter would
/// otherwise fail to `import xonsh`/`import prompt_toolkit` under the fake
/// `HOME`. Resolve the real `HOME`'s user-site-packages directory once (via
/// the interpreter named in xonsh's own shebang line) and pin `PYTHONPATH`
/// to it so xonsh keeps working inside the sandbox -- this exactly mirrors
/// the fixup in the original shell-script harness.
static XONSH_PYTHONPATH: LazyLock<Option<String>> = LazyLock::new(|| {
    let bin = &*XONSH_BIN;
    let shebang = std::fs::read_to_string(bin).ok()?;
    let first_line = shebang.lines().next()?;
    let interp = first_line.strip_prefix("#!")?.trim();
    if !Path::new(interp).is_file() {
        return None;
    }
    let out = Command::new(interp)
        .args(["-c", "import site; print(site.getusersitepackages())"])
        .output()
        .ok()?;
    let site_packages = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if site_packages.is_empty() || !Path::new(&site_packages).is_dir() {
        return None;
    }
    Some(site_packages)
});

/// `starship.toml`'s top-level shape for these checks, composed entirely from
/// starship's own real config structs (`StarshipRootConfig`, `CharacterConfig`,
/// `CustomConfig`, `TimeConfig`) via their own `Serialize` impls -- not a
/// hand-built, stringly-keyed `toml::Table` or hand-typed TOML string. A
/// previous attempt hand-built a generic `toml::Value::Table` and had a real
/// bug (malformed-enough TOML that starship silently fell back to defaults);
/// going through the real `#[derive(Serialize)]` structs makes a typo'd/
/// renamed field a compile error instead of a silently-ignored key.
///
/// This mirrors `super::ScratchConfig`/`super::render_config` exactly, with
/// one addition: a `[time]` module (not a `custom` one), which is this file's
/// unique per-render marker (see [`xonsh_config`] below) -- there's no
/// existing helper for a bare non-custom module in `mod.rs`, so it's composed
/// here alongside the shared pieces via the same `#[serde(flatten)]` pattern.
#[derive(serde::Serialize)]
struct XonshScratchConfig<'a> {
    #[serde(flatten)]
    root: StarshipRootConfig,
    character: CharacterConfig<'a>,
    custom: indexmap::IndexMap<&'a str, CustomConfig<'a>>,
    time: TimeConfig<'a>,
}

/// `starship.toml` with one slow (cacheable, >= `SLOW_MODULE_THRESHOLD`)
/// custom module and one fast one, plus a microsecond-resolution `$time`
/// module, exactly mirroring the original test's scratch config. The `$time`
/// output is our unique per-render marker: a genuine re-render must change
/// it, whereas replayed/cached/unchanged output would not.
fn xonsh_config() -> String {
    xonsh_config_with_refresh(0)
}

/// Like [`xonsh_config`], but also sets the root `refresh_interval` (whole
/// seconds) that drives the live-update tick.
fn xonsh_config_with_refresh(refresh_interval: u64) -> String {
    let mut root = StarshipRootConfig::default();
    root.format = "$custom$character".to_string();
    root.right_format = "$time".to_string();
    root.refresh_interval = refresh_interval;

    let scratch = XonshScratchConfig {
        root,
        character: CharacterConfig {
            success_symbol: "[>](bold green)",
            error_symbol: "[x](bold red)",
            ..Default::default()
        },
        custom: indexmap::IndexMap::from([("slow", custom_module("sleep 0.2 && echo SLOW"))]),
        time: TimeConfig {
            disabled: false,
            format: "[$time]($style) ",
            time_format: Some("%H:%M:%S.%3f"),
            ..Default::default()
        },
    };
    toml::to_string(&scratch).expect("failed to serialize scratch config")
}

struct XonshEnv {
    _workdir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    init_script: PathBuf,
}

impl XonshEnv {
    fn new() -> Self {
        let workdir = tempfile::tempdir().expect("failed to create scratch workdir");
        let home = workdir.path().join("home");
        let config = workdir.path().join("starship.toml");
        let cache = workdir.path().join("cache");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(&config, xonsh_config()).unwrap();

        let init_content = substituted_init_script("src/init/starship.xsh");
        let init_script = workdir.path().join("starship_init.xsh");
        std::fs::write(&init_script, &init_content).unwrap();
        assert!(
            !init_content.contains("::STARSHIP::"),
            "::STARSHIP:: substitution failed"
        );

        Self {
            _workdir: workdir,
            home,
            config,
            cache,
            init_script,
        }
    }

    /// Spawn a fresh, isolated xonsh session (`--no-rc`, its own sandboxed
    /// `$HOME`/`STARSHIP_CACHE`) with `STARSHIP_ASYNC` set to `async_value`,
    /// source the real init script, and let things settle.
    fn spawn(&self, async_value: &str) -> PtySession {
        let cache_dir = self.cache.join(async_value.replace(['/', ' '], "_"));
        self.spawn_with_cache(&cache_dir, async_value)
    }

    /// Like [`Self::spawn`], but with an explicit cache directory (used by
    /// the abrupt-kill check, which needs its own dedicated cache dir).
    fn spawn_with_cache(&self, cache_dir: &Path, async_value: &str) -> PtySession {
        std::fs::create_dir_all(cache_dir).unwrap();
        let mut envs: Vec<(&str, &str)> = vec![
            ("HOME", self.home.to_str().unwrap()),
            ("STARSHIP_CONFIG", self.config.to_str().unwrap()),
            ("STARSHIP_CACHE", cache_dir.to_str().unwrap()),
            ("STARSHIP_ASYNC", async_value),
        ];
        if let Some(pythonpath) = XONSH_PYTHONPATH.as_deref() {
            envs.push(("PYTHONPATH", pythonpath));
        }
        PtySession::spawn(SpawnOptions {
            program: XONSH_BIN.to_str().unwrap(),
            args: &["--no-rc", "--shell-type=prompt_toolkit"],
            envs: &envs,
            cwd: Some(self._workdir.path()),
        })
        .expect("failed to spawn xonsh")
    }

    /// Spawn a session, source the real init script, and let both settle --
    /// the common preamble every check below needs before sending its own
    /// probe(s).
    fn spawn_and_source(&self, async_value: &str) -> PtySession {
        let mut session = self.spawn(async_value);
        session.pump(Duration::from_secs(3));
        session.send(&format!("source {}\r", self.init_script.display()));
        session.pump(Duration::from_millis(500));
        session
    }

    /// Like [`Self::spawn_and_source`], but with an explicit cache dir (used
    /// by the abrupt-kill check).
    fn spawn_and_source_with_cache(&self, cache_dir: &Path, async_value: &str) -> PtySession {
        let mut session = self.spawn_with_cache(cache_dir, async_value);
        session.pump(Duration::from_secs(3));
        session.send(&format!("source {}\r", self.init_script.display()));
        session.pump(Duration::from_millis(500));
        session
    }
}

/// Feed a multi-line Python probe into the live xonsh/prompt_toolkit REPL as
/// a single bracketed paste, then press Enter, then wait for `tag` to show up
/// in the transcript. Returns the extracted `TAG:` payload.
///
/// prompt_toolkit auto-indents continuation lines as they're typed character
/// by character, which fights with any indentation *we* also supply for a
/// multi-line block sent line-by-line: the two stack and quickly desync from
/// what Python's grammar allows, silently discarding the rest of the block
/// with a `SyntaxError`/`IndentationError` (e.g. a `def` body's second
/// statement landing indented deep enough to break the function, leaving it
/// undefined). Real terminals avoid exactly this by wrapping pasted text in
/// `ESC[200~ ... ESC[201~` ("bracketed paste") markers, which the pty
/// transcript confirms xonsh/ptk has already negotiated (`?2004h`); ptk then
/// inserts the payload verbatim in one shot instead of running its
/// per-keystroke auto-indent logic over it. This is the same mechanism a real
/// terminal emulator uses when a user pastes multi-line code, so it's what we
/// replicate here rather than trying to hand-reproduce ptk's auto-indent
/// heuristics line by line. Every multi-line Python block passed in here
/// (`def`, `for`, ...) must be followed by a blank line for this reason.
///
/// The terminal echoes the pasted source verbatim as part of accepting the
/// paste, *before* xonsh actually interprets and runs it -- and the source
/// itself necessarily contains the literal text `print('TAG:' + ...)`, i.e.
/// the tag string appears in the transcript twice: once as this harmless
/// echo, once (later, after real interpreter work -- possibly hundreds of ms
/// for slow-module probes) as the genuine printed output. A naive
/// "wait until `TAG:` appears anywhere" check races the echo and reliably
/// wins, extracting the echoed source line instead of the real payload
/// (looks identical to "the probe never ran" unless you inspect raw bytes).
/// xonsh/ptk always emits `ESC[?2004l` (bracketed-paste-mode-disable)
/// immediately after accepting a pasted block and before running it, so
/// restrict both the wait and the extraction to the transcript *after* this
/// probe's own paste -- guaranteeing only the genuine execution output (if
/// any) can match.
fn probe(session: &mut PtySession, script: &str, tag: &str, timeout: Duration) -> String {
    let script = script.strip_suffix('\r').unwrap_or(script);
    let start = session.raw_transcript().len();
    session.send(&format!("\x1b[200~{script}\x1b[201~\r"));
    let paste_end = "\x1b[?2004l";
    let after_paste = |t: &str| -> Option<String> {
        let region = &t[start.min(t.len())..];
        let post = region
            .rfind(paste_end)
            .map(|i| &region[i + paste_end.len()..])?;
        Some(post.to_string())
    };
    let found = session.wait_until(timeout, |t| {
        after_paste(t).is_some_and(|post| post.contains(&format!("{tag}:")))
    });
    let transcript = session.raw_transcript();
    assert!(
        found,
        "probe tagged {tag:?} never completed; transcript:\n{transcript}"
    );
    let post = after_paste(&transcript).unwrap_or_default();
    extract_tag(&post, tag)
        .unwrap_or_else(|| panic!("no {tag}: payload found in post-paste output: {post:?}"))
}

fn cleanly_exit(session: &mut PtySession) {
    session.send("exit\r");
    session.pump(Duration::from_secs(2));
}

// ============================================================================
// CHECK 1: first paint is synchronous & non-empty.
// ============================================================================

#[test]
fn first_paint_is_synchronous_and_nonempty() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let mut session = env.spawn_and_source("1");

    let probe_script = r#"import json, time
res = {}
res['initial_left'] = _STARSHIP_LEFT
res['initial_right'] = _STARSHIP_RIGHT
res['async_enabled'] = _STARSHIP_ASYNC_ENABLED
print('INITIAL:' + json.dumps(res))
t0 = time.time()
p1 = starship_prompt()
t1 = time.time()
print('FIRST_PROMPT_CALL:' + json.dumps({'value': p1, 'elapsed_ms': (t1 - t0) * 1000}))
"#;
    let initial = probe(
        &mut session,
        probe_script,
        "INITIAL",
        Duration::from_secs(10),
    );
    // Both tags are printed by the same probe submission; wait for the
    // second, later tag directly rather than sending a second probe.
    let found = session.wait_until(Duration::from_secs(10), |t| {
        t.contains("FIRST_PROMPT_CALL:")
    });
    assert!(
        found,
        "no FIRST_PROMPT_CALL payload; transcript:\n{}",
        session.raw_transcript()
    );
    let first_call = session
        .extract_tag("FIRST_PROMPT_CALL")
        .expect("no FIRST_PROMPT_CALL payload");

    let initial_left = json_field(&initial, "initial_left");
    assert!(
        initial_left.is_some_and(|v| !v.is_empty()),
        "no INITIAL payload or empty initial_left: {initial:?}"
    );

    let elapsed_ms: Option<f64> =
        json_field(&first_call, "elapsed_ms").and_then(|v| v.parse().ok());
    assert!(
        elapsed_ms.is_some_and(|ms| ms <= 50.0),
        "payload={first_call:?} (expected a fast, already-cached return)"
    );

    cleanly_exit(&mut session);
    session.kill();
}

// ============================================================================
// CHECK 2: once the cache is warm, the on_pre_prompt background worker
// refreshes _STARSHIP_LEFT/_STARSHIP_RIGHT via --async and app.invalidate()
// fires, and the content actually changes (proven via a unique per-render
// marker: the microsecond `$time` module output).
// ============================================================================

#[test]
fn background_refresh_invalidates_and_updates_content() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let mut session = env.spawn_and_source("1");

    // Split across two separately-submitted commands (probe_a, then probe_b
    // after a real pause) rather than one command that calls `time.sleep()`
    // itself to "wait for the background refresh": xonsh's subprocess-running
    // machinery (`__xonsh__.subproc_captured_stdout`, used by
    // `_starship_refresh`) is not safe to run concurrently from a background
    // thread while the *foreground* interpreter thread is itself blocked
    // inside a Python `time.sleep()` call mid-exec -- confirmed by direct
    // experimentation (an isolated probe hangs forever at exactly this point:
    // state reads fine up to and including 50ms into the sleep, then the
    // session never produces another byte of output, not even much later).
    // This doesn't come up in real interactive use because the *real* main
    // thread is sitting idle in prompt_toolkit's own event loop between
    // commands, not blocked inside an exec'd sleep -- so waiting for the real
    // 2 real-world seconds from the Rust driver side instead (between two
    // separate, quickly-returning commands) exercises the same
    // background-refresh behavior without the artificial deadlock.
    let probe_a = r#"import json
app = __xonsh__.shell.shell.prompter.app
_orig_invalidate = app.invalidate
_calls = {'n': 0}
def _wrapped():
    _calls['n'] += 1
    return _orig_invalidate()

app.invalidate = _wrapped
left_before = _STARSHIP_LEFT
right_before = _STARSHIP_RIGHT
_starship_on_pre_prompt()
print('FIRED:' + json.dumps({'left_before': left_before, 'right_before': right_before}))
"#;
    let probe_b = r#"import json
res2 = {}
res2['invalidate_calls'] = _calls['n']
res2['left_after'] = _STARSHIP_LEFT
res2['right_after'] = _STARSHIP_RIGHT
print('AFTER_REFRESH:' + json.dumps(res2))
p2 = starship_prompt()
rp2 = starship_rprompt()
print('POST_REFRESH_PROMPT_CALL:' + json.dumps({'left': p2, 'right': rp2}))
"#;
    let fired = probe(&mut session, probe_a, "FIRED", Duration::from_secs(10));
    let left_before = json_field(&fired, "left_before").unwrap_or_default();
    let right_before = json_field(&fired, "right_before").unwrap_or_default();

    // Let the background refresh (>= 200ms custom module) actually land while
    // the shell is genuinely idle between commands, exactly like a real
    // interactive session between two prompts.
    session.pump(Duration::from_secs(2));

    let after = probe(
        &mut session,
        probe_b,
        "AFTER_REFRESH",
        Duration::from_secs(10),
    );
    let invalidate_calls: i64 = json_field(&after, "invalidate_calls")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(invalidate_calls >= 1, "invalidate_calls={invalidate_calls}");

    let left_after = json_field(&after, "left_after").unwrap_or_default();
    let right_after = json_field(&after, "right_after").unwrap_or_default();
    let changed = left_after != left_before || right_after != right_before;
    assert!(
        changed,
        "before=({left_before:?},{right_before:?}) after=({left_after:?},{right_after:?})"
    );

    let transcript = session.raw_transcript();
    let post_found = session.wait_until(Duration::from_secs(10), |t| {
        t.contains("POST_REFRESH_PROMPT_CALL:")
    });
    assert!(
        post_found,
        "no POST_REFRESH payload; transcript:\n{transcript}"
    );
    let post = session
        .extract_tag("POST_REFRESH_PROMPT_CALL")
        .expect("no POST_REFRESH payload");
    let post_left = json_field(&post, "left").unwrap_or_default();
    let post_right = json_field(&post, "right").unwrap_or_default();
    assert!(
        post_left == left_after && post_right == right_after,
        "post=({post_left:?},{post_right:?}) vs after=({left_after:?},{right_after:?})"
    );

    cleanly_exit(&mut session);
    session.kill();
}

// ============================================================================
// CHECK 3: on_pre_prompt fires once per logical prompt line, not once per
// resize.
// ============================================================================

#[test]
fn resize_does_not_refire_on_pre_prompt() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let mut session = env.spawn_and_source("1");

    // Blank line after the `def` block, for the same auto-indent reason
    // documented on `probe` above.
    let register_probe = r#"_fire_count = {'n': 0}
@events.on_pre_prompt
def _counter(**kw):
    _fire_count['n'] += 1

print('COUNTER_READY:true')
"#;
    probe(
        &mut session,
        register_probe,
        "COUNTER_READY",
        Duration::from_secs(10),
    );

    // Establish a baseline rather than assuming on_pre_prompt fires exactly
    // zero times between registering the counter and reading it: confirmed by
    // direct experimentation that just returning to an idle prompt after the
    // registration command already ticks the counter (xonsh's own
    // prompt-cycle bookkeeping fires on_pre_prompt more than once per
    // naively-expected "one command, one prompt" cycle -- an internal cadence
    // detail, not something this test should hardcode). What actually matters
    // for "not once per resize" is the *delta* caused specifically by the
    // resizes, not the absolute count.
    session.pump(Duration::from_millis(300));
    let baseline_payload = probe(
        &mut session,
        "print('BASELINE:' + str(_fire_count['n']))",
        "BASELINE",
        Duration::from_secs(10),
    );

    // Real terminal resizes: same TIOCSWINSZ + kernel-delivered SIGWINCH path
    // a real terminal emulator would use.
    for (cols, lines) in [(100, 30), (60, 20), (120, 40), (80, 24)] {
        session.resize(cols, lines);
        session.pump(Duration::from_millis(300));
    }
    session.pump(Duration::from_secs(1));

    let fire_count_payload = probe(
        &mut session,
        "print('FIRE_COUNT:' + str(_fire_count['n']))",
        "FIRE_COUNT",
        Duration::from_secs(10),
    );

    let baseline: Option<i64> = baseline_payload.trim().parse().ok();
    let fire_count: Option<i64> = fire_count_payload.trim().parse().ok();
    // 4 resizes must not each independently tick the counter (that would show
    // up as a delta of ~4); a small, bounded delta (<=1) from whatever the
    // baseline settles at is acceptable and matches what a real
    // terminal-driven reflow-on-resize would look like.
    let delta = fire_count.zip(baseline).map(|(f, b)| f - b);
    assert!(
        delta.is_some_and(|d| (0..=1).contains(&d)),
        "on_pre_prompt count baseline={baseline:?} -> {fire_count:?} (delta={delta:?}) after 4 SIGWINCH resizes, expected delta of 0 or 1"
    );

    cleanly_exit(&mut session);
    session.kill();
}

// ============================================================================
// CHECK 4: STARSHIP_ASYNC=0 fully disables the async machinery. Guards the
// fixed bug where $STARSHIP_ASYNC was hardcoded to '1' with no opt-out; this
// proves the fix genuinely disables everything, not just that the env var
// reads '0'.
// ============================================================================

#[test]
fn disabled_async_flag_is_respected() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let mut session = env.spawn_and_source("0");

    let disabled = probe(
        &mut session,
        r#"import json
res = {}
res['async_enabled_flag'] = _STARSHIP_ASYNC_ENABLED
res['starship_async_env'] = $STARSHIP_ASYNC
res['hook_registered'] = _starship_on_pre_prompt in list(events.on_pre_prompt)
res['initial_left'] = _STARSHIP_LEFT
res['initial_right'] = _STARSHIP_RIGHT
print('DISABLED_STATE:' + json.dumps(res))
"#,
        "DISABLED_STATE",
        Duration::from_secs(10),
    );

    let starship_async_env = json_field(&disabled, "starship_async_env");
    let async_enabled_flag = json_field(&disabled, "async_enabled_flag");
    assert!(
        starship_async_env == Some("0") && async_enabled_flag == Some("false"),
        "payload={disabled:?}"
    );

    let hook_registered = json_field(&disabled, "hook_registered");
    assert!(
        hook_registered == Some("false"),
        "hook_registered={hook_registered:?}"
    );

    let initial_left = json_field(&disabled, "initial_left").unwrap_or("nonempty");
    let initial_right = json_field(&disabled, "initial_right").unwrap_or("nonempty");
    assert!(
        initial_left.is_empty() && initial_right.is_empty(),
        "initial_left={initial_left:?} initial_right={initial_right:?}"
    );

    cleanly_exit(&mut session);
    session.kill();
}

#[test]
fn disabled_async_uses_no_background_thread_and_stays_synchronous() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let mut session = env.spawn_and_source("0");

    let disabled = probe(
        &mut session,
        "print('THREADS_BEFORE:' + str(__import__('threading').active_count()))",
        "THREADS_BEFORE",
        Duration::from_secs(10),
    );
    let before_threads: Option<i64> = disabled.trim().parse().ok();

    let after = probe(
        &mut session,
        r#"import json, time, threading
timings = []
for _ in range(4):
    t0 = time.time()
    p = starship_prompt()
    t1 = time.time()
    rp = starship_rprompt()
    timings.append((t1 - t0) * 1000)

res2 = {}
res2['timings_ms'] = timings
res2['left_still_empty'] = (_STARSHIP_LEFT == '')
res2['right_still_empty'] = (_STARSHIP_RIGHT == '')
res2['thread_count_after'] = threading.active_count()
print('AFTER_CYCLES:' + json.dumps(res2))
"#,
        "AFTER_CYCLES",
        // 4 fully-synchronous Direct-mode renders (each >= 200ms thanks to
        // the slow custom module) plus xonsh/ptk overhead can comfortably
        // exceed a couple of seconds under load -- poll instead of guessing.
        Duration::from_secs(15),
    );

    let after_threads: Option<i64> =
        json_field(&after, "thread_count_after").and_then(|v| v.parse().ok());
    assert!(
        before_threads == Some(1) && after_threads == before_threads,
        "before={before_threads:?} after={after_threads:?}"
    );

    // Old fully-synchronous path: every one of the 4 render cycles pays the
    // full "sleep 0.2" custom-module cost directly (~200ms+), because Direct
    // mode never touches the cache. This is the regression signature: a
    // hardcoded STARSHIP_ASYNC='1' would instead make these calls CacheRead
    // (fast) after the first, or empty/instant if never warmed.
    let timings_raw = json_field(&after, "timings_ms").unwrap_or_default();
    let timings: Vec<f64> = timings_raw
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let slow_enough = !timings.is_empty() && timings.iter().all(|&t| t >= 150.0);
    assert!(
        slow_enough,
        "timings_ms={timings:?} (all expected >= 150ms, matching Direct exec mode)"
    );

    let left_still_empty = json_field(&after, "left_still_empty");
    let right_still_empty = json_field(&after, "right_still_empty");
    assert!(
        left_still_empty == Some("true") && right_still_empty == Some("true"),
        "left_still_empty={left_still_empty:?} right_still_empty={right_still_empty:?}"
    );

    cleanly_exit(&mut session);
    session.kill();
}

// ============================================================================
// CHECK 5: abrupt kill mid-refresh leaves no zombies / corrupted cache.
// ============================================================================

#[test]
fn abrupt_kill_mid_refresh_leaves_no_zombies_or_corrupt_cache() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    let cache_dir = env.cache.join("kill_test");
    let mut session = env.spawn_and_source_with_cache(&cache_dir, "1");
    let xonsh_pid = session.pid();

    session.send("print('READY:true')\n_starship_on_pre_prompt()\r");
    // Give the background thread just enough time to spawn the `starship
    // --async` subprocess (which itself sleeps 0.2s in the slow module) but
    // kill well before it can finish and write cache.
    session.pump(Duration::from_millis(80));
    session.kill();
    std::thread::sleep(Duration::from_millis(500));

    // No zombie/orphaned starship processes descended from *this* test's
    // xonsh session. Scoped by ppid chain rather than a bare system-wide
    // match on the binary's command name: the other checks in this same file
    // now run concurrently (each `#[test]` fn is its own thread under the
    // default multi-threaded runner) and legitimately have their own
    // in-flight `starship --async` refreshes at any given moment, which a
    // bare "any process named .../target/debug/starship" scan would
    // misattribute as leaks from this check.
    let ps_out = Command::new("ps")
        .args(["-eo", "pid,ppid,stat,comm"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let bin_str = STARSHIP_BIN.display().to_string();
    let mut ppid_of: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut starship_pids: Vec<(u32, u32)> = Vec::new();
    for line in ps_out.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(_stat)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        ppid_of.insert(pid, ppid);
        let comm = fields.collect::<Vec<_>>().join(" ");
        if comm == bin_str {
            starship_pids.push((pid, ppid));
        }
    }
    // Walk each starship process's ancestry looking for the xonsh session's
    // pid; a process is "descended from" this test's session if xonsh_pid
    // appears anywhere in its ppid chain.
    let leaked: Vec<u32> = starship_pids
        .into_iter()
        .filter(|&(pid, _)| {
            let mut cur = pid;
            for _ in 0..32 {
                if cur == xonsh_pid {
                    return true;
                }
                match ppid_of.get(&cur) {
                    Some(&parent) if parent != cur => cur = parent,
                    _ => return false,
                }
            }
            false
        })
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        leaked.is_empty(),
        "leaked processes descended from xonsh pid {xonsh_pid}: {leaked:?}"
    );

    // Cache directory must contain only well-formed entries: any `.tmp`/
    // dotfiles are harmless orphans, never renamed over a real entry; the
    // important property is that no partially-written *final* (non-dot)
    // cache file exists that fails to parse as JSON.
    let mut corrupted = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(data) => {
                    if serde_json::from_str::<serde_json::Value>(&data).is_err() {
                        corrupted.push(name);
                    }
                }
                Err(e) => corrupted.push(format!("{name}: {e}")),
            }
        }
    }
    assert!(corrupted.is_empty(), "corrupted entries={corrupted:?}");
}

// ============================================================================
// CHECK: live updates -- with refresh_interval > 0 the background ticker
// recomputes the prompt and invalidates prompt_toolkit on its own timer, so
// the live `time` module (right prompt, microsecond format) advances while the
// user sits idle, with no keypress.
// ============================================================================

#[test]
fn live_tick_advances_time_module_while_idle() {
    let _ = &*STARSHIP_BIN;
    let env = XonshEnv::new();
    // The default config has refresh_interval = 0; enable the tick.
    std::fs::write(&env.config, xonsh_config_with_refresh(1)).unwrap();

    let mut session = env.spawn_and_source("1");
    // Idle: send nothing. Poll until the ticker has repainted at least three
    // distinct microsecond clock values on its own.
    let clock = r"\d\d:\d\d:\d\d\.\d{3}";
    let ticked =
        session.wait_until(Duration::from_secs(10), |t| super::unique_matches(t, clock).len() >= 3);

    cleanly_exit(&mut session);

    assert!(
        ticked,
        "xonsh prompt did not repaint on its own timer: fewer than 3 distinct clock values while idle (saw {:?})",
        super::unique_matches(&session.raw_transcript(), clock)
    );
}
