//! Automated, self-contained regression test for the async prompt
//! integration in `src/init/starship.zsh`. Drives a real interactive zsh
//! (over a real pty, since `zle` / `zle -F` require a genuine tty) against
//! the real built starship binary, and asserts the end-to-end async
//! behavior documented in that file.

use super::{
    CharacterConfig, CustomConfig, PtySession, SpawnOptions, STARSHIP_BIN, custom_module, render_config,
    substituted_init_script, unique_matches,
};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// A `[custom.<name>]` entry for zsh's own scratch configs, which need
/// per-module `style`/`format` overrides that [`custom_module`]'s defaults
/// don't cover (e.g. distinct colors per side, no trailing space on the
/// right-side module so it doesn't leave stray whitespace at the end of
/// the line). Starts from the same real-bash-backed [`custom_module`]
/// default and overrides just the fields zsh's configs care about, so it's
/// still the real `CustomConfig` struct (a typo'd field is a compile
/// error), not hand-typed TOML.
fn zsh_custom_module<'a>(command: &'a str, style: &'a str, format: &'a str) -> CustomConfig<'a> {
    CustomConfig { style, format, ..custom_module(command) }
}

/// One fast module, plus a module exceeding `SLOW_MODULE_THRESHOLD` (5ms) on
/// each of `PROMPT` and `RPROMPT`, each emitting a unique marker per render,
/// so we can tell "computed live just now" apart from "served from cache"
/// apart from "recomputed by the background refresh".
fn config_both() -> String {
    render_config(
        "$custom$character",
        Some("$custom_r"),
        false,
        &[
            ("fast", zsh_custom_module("echo FAST", "green", "[$output]($style) ")),
            (
                "slow",
                zsh_custom_module("sleep 0.2 && echo SLOW-$(date +%s%N)", "yellow", "[$output]($style) "),
            ),
            (
                "r",
                zsh_custom_module("sleep 0.2 && echo RSLOW-$(date +%s%N)", "magenta", "[$output]($style)"),
            ),
        ],
        Some(CharacterConfig { success_symbol: "[>](bold green)", ..Default::default() }),
    )
}

/// Only the right side (`RPROMPT`) has a slow module; the left side is fast.
fn config_right_only() -> String {
    render_config(
        "$custom",
        Some("$custom_r"),
        false,
        &[
            ("fast", zsh_custom_module("echo LFAST", "green", "[$output]($style) ")),
            (
                "r",
                zsh_custom_module("sleep 0.2 && echo RSLOW-$(date +%s%N)", "magenta", "[$output]($style)"),
            ),
        ],
        None,
    )
}

/// Only the left side (`PROMPT`) has a slow module; the right side is fast.
fn config_left_only() -> String {
    render_config(
        "$custom",
        Some("$custom_r"),
        false,
        &[
            (
                "slow",
                zsh_custom_module("sleep 0.2 && echo LSLOW-$(date +%s%N)", "green", "[$output]($style) "),
            ),
            ("r", zsh_custom_module("echo RFAST", "magenta", "[$output]($style)")),
        ],
        None,
    )
}

/// A scratch zsh environment: an isolated `ZDOTDIR` plus the real, already
/// `::STARSHIP::`-substituted `starship.zsh` init script -- everything each
/// check needs to spin up its own freshly configured, freshly cached zsh
/// session.
struct ZshEnv {
    workdir: tempfile::TempDir,
    zdotdir: PathBuf,
    init_script: PathBuf,
}

impl ZshEnv {
    fn new() -> Self {
        let workdir = tempfile::tempdir().expect("failed to create scratch workdir");
        let zdotdir = workdir.path().join("zdotdir");
        fs::create_dir_all(&zdotdir).unwrap();

        let init_content = substituted_init_script("src/init/starship.zsh");
        let init_script = workdir.path().join("init.zsh");
        fs::write(&init_script, &init_content).unwrap();
        assert!(
            init_content.contains(&STARSHIP_BIN.display().to_string()),
            "::STARSHIP:: substitution failed"
        );

        Self { workdir, zdotdir, init_script }
    }

    /// Write a `.zshrc`/`.zshenv` sourcing the init script with the given
    /// config and `STARSHIP_ASYNC` setting, using a fresh cache dir, and
    /// spawn an interactive zsh over a real pty. Blocks until the shell has
    /// actually finished sourcing the init script and painted its first
    /// prompt (see the `ZSHRC_READY` marker below) rather than a fixed
    /// sleep, since under the default parallel test runner several of
    /// these pty-backed tests spawn their own interactive zsh + starship
    /// processes at once, and fork/exec contention on a loaded machine can
    /// make shell startup itself take much longer than a short fixed wait
    /// budget -- proceeding before the shell is actually ready otherwise
    /// makes every subsequent send/expect in the test race the shell's own
    /// startup.
    fn spawn(&self, config: &str, async_value: &str) -> PtySession {
        let cache_dir = self.workdir.path().join(format!("cache_{}", uniquify()));
        fs::create_dir_all(&cache_dir).unwrap();

        let config_path = self.zdotdir.join("starship.toml");
        fs::write(&config_path, config).unwrap();
        fs::write(
            self.zdotdir.join(".zshrc"),
            format!(
                r#"export STARSHIP_CONFIG="{config}"
export STARSHIP_CACHE="{cache}"
export STARSHIP_ASYNC="{async_value}"
export PS1='local%% '
source "{init}"
echo ZSHRC_READY
"#,
                config = config_path.display(),
                cache = cache_dir.display(),
                init = self.init_script.display(),
            ),
        )
        .unwrap();
        fs::write(self.zdotdir.join(".zshenv"), "").unwrap();

        let mut session = PtySession::spawn(SpawnOptions {
            program: "zsh",
            args: &["-i"],
            envs: &[("ZDOTDIR", &self.zdotdir.display().to_string())],
            cwd: Some(&self.zdotdir),
        })
        .expect("failed to spawn zsh");

        let ready = session.wait_until(Duration::from_secs(20), |t| t.contains("ZSHRC_READY"));
        assert!(
            ready,
            "zsh did not finish sourcing .zshrc within 20s (transcript so far: {:?})",
            session.raw_transcript()
        );
        // The marker itself, and the first prompt draw it triggers, may
        // still be trickling in right after the echo lands -- give both a
        // moment to settle so callers see a fully painted first prompt.
        session.pump(Duration::from_millis(300));

        session
    }
}

/// A small monotonic counter to keep each spawned session's cache dir
/// distinct even when several sessions are spawned from the same [`ZshEnv`]
/// within one test.
fn uniquify() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Instant `--cached` paint shows before the background refresh completes,
/// and once the single `--deferred --watch` watcher's refresh lands, its poke
/// genuinely redraws the prompt in place via `zle -F` + `zle reset-prompt`
/// (proven via unique per-render markers) -- on both `PROMPT` and `RPROMPT`
/// from the one shared refresh.
#[test]
fn instant_paint_then_redraw_in_place_on_both_sides() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_both(), "1");
    session.send_and_pump("echo BEGIN_CYCLE\r", Duration::from_millis(500));
    // Give the background refresh (sleep 0.2 in each module) time to land
    // and fire its zle -F callback before we look at the redrawn prompt.
    // Poll for it rather than a fixed sleep, so this doesn't flake under
    // system load. Wait for a *second* distinct marker on each side (proving
    // a real redraw happened, not just the initial paint) rather than for
    // any specific text, since that's what this check is actually about.
    session.wait_until(Duration::from_secs(8), |t| {
        left_slow_markers(t).len() >= 2 && unique_matches(t, r"RSLOW-[0-9]+").len() >= 2
    });
    session.send_and_pump("echo AFTER_SETTLE\r", Duration::from_millis(500));
    session.pump(Duration::from_secs(1));
    session.send_and_pump("echo FINAL_CYCLE\r", Duration::from_millis(500));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));

    let transcript = session.raw_transcript();

    // Extract every SLOW-<digits> and RSLOW-<digits> marker in the order
    // they appeared. Because empty-cache CacheRead computes missing
    // entries live, the very first paint already contains a freshly
    // computed marker; the meaningful assertion is that a *different*
    // marker appears later in the same pty session once the background
    // refresh (with its own live compute) lands and triggers a redraw --
    // i.e. the visible content actually changes without the user pressing
    // Enter again.
    let slow_markers = left_slow_markers(&transcript);
    let rslow_markers = unique_matches(&transcript, r"RSLOW-[0-9]+");

    assert!(
        !slow_markers.is_empty(),
        "instant CacheRead paint did not appear: no SLOW marker ever appeared in transcript"
    );
    assert!(
        slow_markers.len() >= 2,
        "PROMPT was not redrawn in place with new content after async refresh (>=2 distinct SLOW markers expected, saw {}); zle -F redraw not proven",
        slow_markers.len()
    );
    assert!(
        rslow_markers.len() >= 2,
        "RPROMPT was not redrawn in place with new content after async refresh (>=2 distinct RSLOW markers expected, saw {}); zle -F redraw not proven",
        rslow_markers.len()
    );
}

/// With only the right side (`RPROMPT`) slow, it still refreshes
/// independently and repeatedly even though `PROMPT` has no slow module of
/// its own.
///
/// The FIRST prompt draw (from precmd firing at shell startup, before any
/// command is typed) already launches a background refresh. Every
/// subsequent command triggers a new precmd cycle, which cancels any
/// refresh still in flight from the previous cycle. To avoid a race where a
/// fast typist (or a loaded CI box) types the next command -- and so
/// cancels the prior refresh -- before its callback has had a chance to
/// land, we wait generously (well beyond the module's 0.2s sleep plus
/// process fork/exec overhead) both right after the shell spawns and after
/// each subsequent command.
#[test]
fn rprompt_refreshes_independently_when_prompt_has_no_slow_module() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_right_only(), "1");
    session.pump(Duration::from_secs(5));
    session.send_and_pump("echo M1\r", Duration::from_secs(5));
    session.send_and_pump("echo M2\r", Duration::from_secs(5));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));

    let transcript = session.raw_transcript();
    let r_only_markers = unique_matches(&transcript, r"RSLOW-[0-9]+");
    assert!(
        r_only_markers.len() >= 2 && transcript.contains("LFAST"),
        "RPROMPT did not refresh independently when PROMPT has no slow module (distinct RSLOW markers: {})",
        r_only_markers.len()
    );
}

/// With only the left side (`PROMPT`) slow, it still refreshes
/// independently and repeatedly even though `RPROMPT` has no slow module of
/// its own. See [`rprompt_refreshes_independently_when_prompt_has_no_slow_module`]
/// for why the waits are generous.
#[test]
fn prompt_refreshes_independently_when_rprompt_has_no_slow_module() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_left_only(), "1");
    session.pump(Duration::from_secs(5));
    session.send_and_pump("echo M1\r", Duration::from_secs(5));
    session.send_and_pump("echo M2\r", Duration::from_secs(5));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));

    let transcript = session.raw_transcript();
    let l_only_markers = unique_matches(&transcript, r"LSLOW-[0-9]+");
    assert!(
        l_only_markers.len() >= 2 && transcript.contains("RFAST"),
        "PROMPT did not refresh independently when RPROMPT has no slow module (distinct LSLOW markers: {})",
        l_only_markers.len()
    );
}

/// Rapid successive prompt cycles must leave no fd leaks and no zombie
/// processes behind.
#[test]
fn no_fd_leak_or_zombies_across_rapid_cycles() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_both(), "1");

    session.send("echo PIDIS=$$\r");
    // Generous timeout: under the default parallel test runner, several of
    // these pty-backed tests spawn their own interactive zsh + starship
    // processes at once, and fork/exec contention on a loaded machine can
    // otherwise make even this trivial echo round-trip take longer than a
    // short fixed wait budget.
    session.wait_until(Duration::from_secs(15), |t| t.contains("PIDIS="));
    let shellpid = unique_matches(&session.raw_transcript(), r"PIDIS=(\d+)")
        .into_iter()
        .next()
        .map(|m| m.trim_start_matches("PIDIS=").to_string())
        .expect("could not capture shell pid");

    session.send(&format!("lsof -p {shellpid} 2>/dev/null | wc -l\r"));
    session.wait_until(Duration::from_secs(5), |_| true);
    session.pump(Duration::from_millis(300));
    let fd_before = last_integer_line(&session.raw_transcript());

    for i in 1..=20 {
        session.send_and_pump(&format!("echo CYCLE{i}\r"), Duration::from_millis(150));
    }

    // Let the last refresh settle so its fd is actually closed. Generous:
    // under the default parallel test runner, fork/exec and fd-close
    // latency for this session's own subprocesses can stretch out under
    // system-wide contention from every other pty-backed test running at
    // the same time.
    session.pump(Duration::from_secs(2));

    session.send(&format!("lsof -p {shellpid} 2>/dev/null | wc -l\r"));
    session.pump(Duration::from_millis(500));
    let fd_after = last_integer_line(&session.raw_transcript());

    session.send(&format!(
        "ps -ax -o pid,ppid,stat,command | awk -v p={shellpid} '$2 == p {{print}}'\r"
    ));
    session.pump(Duration::from_millis(500));

    session.send("exit\r");
    session.pump(Duration::from_secs(2));
    let transcript = session.raw_transcript();

    match (fd_before, fd_after) {
        (Some(before), Some(after)) => {
            // A small amount of headroom absorbs incidental fd churn under
            // heavy concurrent system load without masking a genuine leak,
            // which would grow roughly with the cycle count (20), not by 1-2.
            assert!(
                after <= before + 4,
                "possible fd leak across 20 rapid prompt cycles (lsof count before={before} after={after})"
            );
        }
        _ => panic!("could not determine fd counts from transcript"),
    }

    let zombie_re = regex::Regex::new(r"<defunct>|Z\+?\s").unwrap();
    assert!(
        !zombie_re.is_match(&transcript),
        "zombie (<defunct>) process detected after rapid prompt cycles"
    );
}

/// `STARSHIP_ASYNC=0` must reproduce the exact old synchronous behavior,
/// with none of the async functions/state defined.
#[test]
fn disabled_async_installs_no_functions_or_state() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_both(), "0");
    session.send_and_pump(
        "print -r -- ASYNC_FN_COUNT_IS:$(typeset -f | grep -c _starship_defer):END\r",
        Duration::from_millis(500),
    );
    session.send_and_pump(
        "print -r -- FDVAR_IS:$_STARSHIP_DEFER_FD:END\r",
        Duration::from_millis(500),
    );
    session.send_and_pump("echo M1\r", Duration::from_secs(1));
    session.send_and_pump("echo M2\r", Duration::from_millis(500));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));
    let transcript = session.raw_transcript();

    // `print -r -- ASYNC_FN_COUNT_IS:<n>:END` puts the count on a single,
    // unambiguous, ANSI-noise-free line regardless of how the pty
    // echoes/redraws the command itself.
    let async_fn_count = regex::Regex::new(r"ASYNC_FN_COUNT_IS:(\d+):END")
        .unwrap()
        .captures(&transcript)
        .map(|c| c[1].to_string());
    assert_eq!(
        async_fn_count.as_deref(),
        Some("0"),
        "async functions were defined under STARSHIP_ASYNC=0 (count={async_fn_count:?})"
    );

    // FDVAR_IS:<value>:END -- an unset variable expands to empty, so we
    // expect the literal marker "FDVAR_IS::END" (nothing between the
    // colons).
    if !transcript.contains("FDVAR_IS::END") {
        let seen = regex::Regex::new(r"FDVAR_IS:[^:]*:END")
            .unwrap()
            .find(&transcript)
            .map(|m| m.as_str().to_string());
        panic!("async fd/state variable was unexpectedly set under STARSHIP_ASYNC=0 (saw: {seen:?})");
    }
}

/// Under `STARSHIP_ASYNC=0` every prompt draw is a plain synchronous
/// `$(...)` via `promptsubst`, so SLOW/RSLOW markers should still appear and
/// differ between successive draws (proving the classic synchronous path
/// still works), with no instant/async-cache two-stage behavior involved.
#[test]
fn disabled_async_still_renders_synchronously() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_both(), "0");
    session.send_and_pump("echo M1\r", Duration::from_secs(1));
    session.send_and_pump("echo M2\r", Duration::from_millis(500));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));
    let transcript = session.raw_transcript();

    let off_slow_markers = left_slow_markers(&transcript);
    assert!(
        off_slow_markers.len() >= 2,
        "classic synchronous PROMPT rendering broken under STARSHIP_ASYNC=0 ({} distinct renders observed)",
        off_slow_markers.len()
    );
}

/// `PROMPT2` / `--continuation` is evaluated eagerly ONCE at init, not
/// lazily re-spawned per continuation line. This is the regression that was
/// fixed (`PROMPT2="$(...)"` vs `PROMPT2='$(...)'`).
///
/// `typeset -p` reveals whether `PROMPT2` holds a literal (already-
/// substituted) string (eager) or a live `$(...)` command substitution
/// (lazy) -- the definitive, deterministic check: it prints the *stored*
/// value; if it still contains `$(` the assignment was lazy (re-evaluated
/// on every draw, each spawning a fresh `starship` process per
/// continuation line). If eager, the stored value is just the rendered
/// prompt text with no `$(`.
#[test]
fn continuation_prompt_is_eager_literal_not_lazy_substitution() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_both(), "1");
    session.send_and_pump("typeset -p PROMPT2\r", Duration::from_millis(500));

    // Open a 3-line continuation (two continuation-prompt draws).
    session.send_and_pump("echo 'first \\\r", Duration::from_millis(400));
    session.send_and_pump("second \\\r", Duration::from_millis(400));
    session.send_and_pump("third'\r", Duration::from_millis(500));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));
    let transcript = session.raw_transcript();

    let prompt2_line = transcript
        .lines()
        .rfind(|l| l.trim_start().starts_with("typeset PROMPT2="))
        .map(str::to_string);
    match &prompt2_line {
        None => panic!("could not capture 'typeset -p PROMPT2' output from transcript"),
        Some(line) if line.contains("$(") => {
            panic!("PROMPT2 still holds a live command substitution (lazy) -- regression is present: {line}")
        }
        Some(_) => {}
    }
}

/// Belt-and-suspenders companion to
/// [`continuation_prompt_is_eager_literal_not_lazy_substitution`]: directly
/// count starship process spawns attributable to `--continuation` during
/// the continuation-line typing, using process accounting via a light
/// poller thread running concurrently with the pty session. One-time eager
/// eval happens at shell init (before we start polling), so we expect ZERO
/// further `--continuation` spawns observed *during* the multi-line typing
/// captured by the poller.
#[test]
fn continuation_prompt_is_not_respawned_per_line() {
    let env = ZshEnv::new();
    let bin = STARSHIP_BIN.as_path();
    let bin_continuation_pattern = format!("{} prompt --continuation", bin.display());

    let mut session = env.spawn(&config_both(), "1");
    session.send("echo PIDIS=$$\r");
    // See the matching comment in `no_fd_leak_or_zombies_across_rapid_cycles`
    // for why this timeout is generous.
    session.wait_until(Duration::from_secs(15), |t| t.contains("PIDIS="));
    let shellpid = unique_matches(&session.raw_transcript(), r"PIDIS=(\d+)")
        .into_iter()
        .next()
        .map(|m| m.trim_start_matches("PIDIS=").to_string())
        .expect("could not capture shell pid");

    // Scope the poller to processes whose PPID is this specific shell, not
    // just any process anywhere matching the binary path -- other shell
    // tests' sessions may be spawning their own `--continuation` calls
    // concurrently on the same machine, and a bare pattern match on the
    // command line would misattribute those to *this* test (see
    // `BashEnv::leaked_pids` in bash.rs for the same concern there, scoped
    // by cwd instead since bash has no equivalent single owning shell pid
    // to poll against as directly).
    let poll_seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let poll_seen_clone = poll_seen.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let poller = std::thread::spawn(move || {
        let mut seen_pids = std::collections::BTreeSet::new();
        while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
            let output = std::process::Command::new("ps").args(["-ax", "-o", "pid,ppid,command"]).output();
            if let Ok(output) = output {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let mut fields = line.split_whitespace();
                    let pid = fields.next();
                    let ppid = fields.next();
                    let command = fields.collect::<Vec<_>>().join(" ");
                    if ppid == Some(shellpid.as_str())
                        && command.contains(&bin_continuation_pattern)
                        && let Some(pid) = pid
                    {
                        seen_pids.insert(pid.to_string());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        poll_seen_clone.store(seen_pids.len(), std::sync::atomic::Ordering::Relaxed);
    });

    session.send_and_pump("echo 'a \\\r", Duration::from_millis(300));
    session.send_and_pump("b \\\r", Duration::from_millis(300));
    session.send_and_pump("c \\\r", Duration::from_millis(300));
    session.send_and_pump("d'\r", Duration::from_millis(500));
    session.send("exit\r");
    session.pump(Duration::from_secs(2));

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    poller.join().ok();
    let cont_spawns = poll_seen.load(std::sync::atomic::Ordering::Relaxed);

    // Allow a small margin (<=1) for a poll window that happens to catch
    // the tail of the initial init-time spawn on a slow CI box, since the
    // poller starts concurrently with, not strictly after, init.
    assert!(
        cont_spawns <= 1,
        "lazy re-evaluation regression: 'starship prompt --continuation' was observed spawning {cont_spawns} times while typing a multi-line command (expected <=1)"
    );
}

/// The last standalone integer appearing on its own line in `text` (used to
/// pull an `lsof | wc -l` result out of a pty transcript, which echoes the
/// command itself before the numeric result line).
fn last_integer_line(text: &str) -> Option<u32> {
    text.lines().rev().find_map(|l| l.trim().parse::<u32>().ok())
}

/// Distinct `SLOW-<digits>` markers, excluding `RSLOW-<digits>` markers.
/// `SLOW-[0-9]+` on its own would also match the tail substring of every
/// `RSLOW-<digits>` marker (regex matching doesn't require word
/// boundaries), so this takes a set difference: every `SLOW-<digits>` match
/// minus every `RSLOW-<digits>` match with its leading `R` stripped.
fn left_slow_markers(text: &str) -> std::collections::BTreeSet<String> {
    let all_slow = unique_matches(text, r"SLOW-[0-9]+");
    let rslow_suffixes: std::collections::BTreeSet<String> = unique_matches(text, r"RSLOW-[0-9]+")
        .into_iter()
        .map(|m| m.trim_start_matches('R').to_string())
        .collect();
    all_slow.difference(&rslow_suffixes).cloned().collect()
}

/// Distinct `HH:MM:SS` clock values painted so far -- how many times the live
/// `time` module has advanced on screen.
fn clock_values(text: &str) -> std::collections::BTreeSet<String> {
    unique_matches(text, r"\d\d:\d\d:\d\d")
}

/// A config whose only visible module is the real `time` module -- which is
/// fast, so it is never recorded in the async cache and is recomputed live on
/// every paint -- plus a `refresh_interval`, so a live tick makes the
/// displayed clock advance without any user input.
fn config_time(refresh_interval: u64) -> String {
    format!(
        r#"add_newline = false
refresh_interval = {refresh_interval}
format = "$time$character"
[time]
disabled = false
format = "[$time]($style) "
time_format = "%H:%M:%S"
[character]
success_symbol = "[>](green)"
"#
    )
}

/// With `refresh_interval > 0`, the `--deferred --watch` watcher keeps
/// emitting poke lines while the user sits idle (no keypress), each one
/// repainted by the shared `zle -F` callback, so a live module like `time`
/// advances -- the clock shows more than one distinct value across a few
/// seconds of idling. This is the core "live update" behavior.
#[test]
fn live_tick_advances_time_module_while_idle() {
    let env = ZshEnv::new();
    let mut session = env.spawn(&config_time(1), "1");
    // Idle: send nothing at all. Poll (which just drains pty output) until at
    // least two distinct clock values have been painted by the timer.
    let ticked = session.wait_until(Duration::from_secs(8), |t| clock_values(t).len() >= 2);
    session.send("exit\r");
    session.pump(Duration::from_secs(1));
    assert!(
        ticked,
        "prompt did not repaint on its own timer: fewer than 2 distinct clock values while idle (saw {:?})",
        clock_values(&session.raw_transcript())
    );
}

/// The tick lives inside the watcher, which reads `refresh_interval` itself:
/// with the default `0` the watcher exits right after its refresh poke, and
/// with a positive value it stays alive ticking until the shell replaces or
/// cancels it -- and never outlives the session (zshexit cleanup).
#[test]
fn watcher_lifetime_follows_refresh_interval() {
    let env = ZshEnv::new();
    let watch_pattern = format!("{} prompt --deferred --watch", STARSHIP_BIN.display());

    // Poll rather than a single fixed pump: under the default parallel test
    // runner, several pty-backed tests spawn their own zsh + starship
    // processes at once, and fork/exec contention on a loaded machine can
    // make a fixed short wait budget flake (see the similar comments
    // elsewhere in this file on why waits here are generous).
    let watcher_present = |session: &mut PtySession, timeout: Duration| -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !super::pids_matching_with_cwd_under(&watch_pattern, env.workdir.path()).is_empty() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            session.pump(Duration::from_millis(200));
        }
    };

    let mut off = env.spawn(&config_time(0), "1");
    // Give the one-shot refresh ample time to land and exit, then confirm it
    // stays gone (rather than just "hasn't appeared yet").
    off.pump(Duration::from_secs(3));
    assert!(
        super::pids_matching_with_cwd_under(&watch_pattern, env.workdir.path()).is_empty(),
        "watcher still alive despite refresh_interval=0: it should exit right after its refresh poke"
    );
    off.send("exit\r");
    off.pump(Duration::from_secs(1));

    let mut on = env.spawn(&config_time(1), "1");
    assert!(
        watcher_present(&mut on, Duration::from_secs(10)),
        "no live watcher with refresh_interval=1: ticks have nothing to drive them"
    );
    on.send("exit\r");
    on.pump(Duration::from_secs(2));
    // zshexit's cancel must reap the ticking watcher; an orphan would also
    // die on its next poke (EPIPE), so poll briefly rather than asserting
    // instantly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut leftover = super::pids_matching_with_cwd_under(&watch_pattern, env.workdir.path());
    while !leftover.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        leftover = super::pids_matching_with_cwd_under(&watch_pattern, env.workdir.path());
    }
    assert!(
        leftover.is_empty(),
        "watcher outlived the zsh session: {leftover:?}"
    );
}
