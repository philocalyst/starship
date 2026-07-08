//! Shared PTY + terminal-emulation test harness for the async prompt
//! integration tests covering `src/init/starship.*` (the `STARSHIP_ASYNC`
//! opt-in fast-paint/background-refresh mechanism described in
//! `src/cache.rs`).
//!
//! Spawns a real shell in a real pseudo-terminal via `alacritty_terminal`'s
//! `tty` module (the same PTY-spawning code Alacritty itself uses), and
//! drives a full `alacritty_terminal::Term` + `vte::ansi::Processor` over its
//! output so escape sequences -- including cursor-position-report queries
//! (`ESC[6n`), which PSReadLine, zle, and other line editors rely on to
//! avoid stalling -- are answered exactly the way a real terminal emulator
//! would (see `Term::device_status`), rather than a hand-rolled fake reply.
//! This intentionally replaces an earlier ad hoc Python `pty` + regex-based
//! harness: no Python, and real terminal emulation instead of a naive
//! ANSI-escape stripper.
//!
//! `clippy::disallowed_methods` (project-wide: `std::process::Command::new`
//! may inadvertently run an executable from the current working directory)
//! is allowed for this whole module tree: unlike the production code path
//! that lint protects, these tests exist specifically to locate and invoke
//! real system shells/tools by name (`bash`, `zsh`, `fish`, `elvish`,
//! `xonsh`, `pwsh`, `ps`, `lsof`, `kill`, `brew`, ...) to drive genuine
//! end-to-end shell integrations -- `crate::utils::create_command`'s
//! CWD-hijack protection isn't the right fit here.
#![allow(clippy::disallowed_methods)]

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, EventedReadWrite, Options as PtyOptions, Shell as PtyShell};
use alacritty_terminal::vte::ansi::Processor;
use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub mod bash;
pub mod elvish;
pub mod fish;
pub mod lua_static;
pub mod ps1;
pub mod xonsh;
pub mod zsh;

/// Fixed dimensions for test sessions: wide enough that rendered prompts
/// don't wrap unexpectedly, tall enough to keep a handful of scrollback
/// lines visible on screen at once.
const COLUMNS: usize = 200;
const SCREEN_LINES: usize = 40;

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Clone)]
struct ChannelListener(mpsc::Sender<Event>);

impl EventListener for ChannelListener {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// Options for spawning a [`PtySession`].
pub struct SpawnOptions<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub envs: &'a [(&'a str, &'a str)],
    pub cwd: Option<&'a Path>,
}

/// A live shell session driven over a real PTY, with a full terminal
/// emulator (not just a naive ANSI-stripper) tracking what's actually
/// visible on screen -- including auto-answering cursor-position-report
/// queries via `Term`'s own `device_status` handling, exactly like a real
/// terminal would.
pub struct PtySession {
    pty: tty::Pty,
    parser: Processor,
    term: Term<ChannelListener>,
    events: mpsc::Receiver<Event>,
    /// Full raw byte transcript across the whole session (pre-terminal-
    /// emulation), so tests can grep for markers across scrollback the
    /// visible screen has already scrolled past.
    raw_log: Vec<u8>,
}

impl PtySession {
    pub fn spawn(opts: SpawnOptions) -> std::io::Result<Self> {
        let mut env = HashMap::new();
        for (k, v) in opts.envs {
            env.insert((*k).to_string(), (*v).to_string());
        }
        // Lets PSReadLine/readline/zle/etc. use their normal full-featured
        // redraw path instead of a dumb-terminal fallback.
        env.entry("TERM".to_string())
            .or_insert_with(|| "xterm-256color".to_string());

        let pty_options = PtyOptions {
            shell: Some(PtyShell::new(
                opts.program.to_string(),
                opts.args.iter().map(|s| (*s).to_string()).collect(),
            )),
            working_directory: opts.cwd.map(Path::to_path_buf),
            drain_on_exit: false,
            env,
        };

        let window_size = WindowSize {
            num_lines: SCREEN_LINES as u16,
            num_cols: COLUMNS as u16,
            cell_width: 8,
            cell_height: 16,
        };

        let pty = tty::new(&pty_options, window_size, 0)?;

        let (tx, rx) = mpsc::channel();
        let listener = ChannelListener(tx);
        let size = TermSize {
            columns: COLUMNS,
            screen_lines: SCREEN_LINES,
        };
        let term = Term::new(Config::default(), &size, listener);

        Ok(Self {
            pty,
            parser: Processor::new(),
            term,
            events: rx,
            raw_log: Vec::new(),
        })
    }

    /// Write a string to the pty as if typed by a user.
    ///
    /// The pty master fd is non-blocking (set by `tty::new` itself), so a
    /// write larger than the pty's input queue can currently hold returns
    /// `WouldBlock` for the remainder -- `Write::write_all` does not retry
    /// on `WouldBlock`, it just stops and returns an error. A large payload
    /// (a multi-line pasted probe, a long command line) can therefore be
    /// silently truncated if written via a single `write_all` call whose
    /// result is discarded, with no visible symptom other than "the rest of
    /// the input never arrived" -- which looks identical to "the shell
    /// hung" from the test's point of view. Loop and retry on `WouldBlock`
    /// (draining any pty *output* in between, so the child isn't stalled
    /// writing to a full output pipe while we're stalled writing to a full
    /// input queue) until every byte is actually written.
    pub fn send(&mut self, s: &str) {
        let mut remaining = s.as_bytes();
        while !remaining.is_empty() {
            match self.pty.writer().write(remaining) {
                Ok(0) => break,
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.pump(Duration::from_millis(10));
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        let _ = self.pty.writer().flush();
    }

    /// Drain any pending output for up to `duration`, feeding it through the
    /// terminal emulator (which also auto-answers escape-sequence queries
    /// like cursor-position reports, exactly like a real terminal) and
    /// appending it to the raw transcript.
    pub fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut buf = [0u8; 65536];
        loop {
            match self.pty.reader().read(&mut buf) {
                Ok(0) => break, // EOF: child exited and closed its end.
                Ok(n) => {
                    self.raw_log.extend_from_slice(&buf[..n]);
                    self.parser.advance(&mut self.term, &buf[..n]);
                    self.flush_pty_writes();
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(_) => break,
            }
        }
    }

    /// Send input, then pump for `duration` -- the common "type a command
    /// and wait for the response" pattern.
    pub fn send_and_pump(&mut self, s: &str, duration: Duration) {
        self.send(s);
        self.pump(duration);
    }

    /// Poll `predicate` against the accumulating raw transcript every 50ms
    /// until it returns `true` or `timeout` elapses. Returns whether the
    /// predicate was satisfied. Prefer this over a fixed `pump` duration
    /// when a check can be satisfied earlier than the worst case, to keep
    /// the suite fast without flaking on slower machines.
    pub fn wait_until(&mut self, timeout: Duration, mut predicate: impl FnMut(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump(Duration::from_millis(50));
            if predicate(&self.raw_transcript()) {
                return true;
            }
            if Instant::now() >= deadline {
                return predicate(&self.raw_transcript());
            }
        }
    }

    /// Write back any `Event::PtyWrite` the terminal emulator generated in
    /// response to escape sequences it just processed (cursor-position
    /// reports, DA/DSR queries, etc.) -- this is what keeps PSReadLine and
    /// other line editors that probe the terminal from stalling.
    fn flush_pty_writes(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let Event::PtyWrite(text) = event {
                let _ = self.pty.writer().write_all(text.as_bytes());
            }
        }
    }

    /// The currently visible screen, as plain text with trailing whitespace
    /// trimmed from each line -- i.e. what a user would actually see after
    /// real terminal emulation (cursor movement, overwrites, clears), not a
    /// naive escape-code strip of the raw byte stream.
    pub fn visible_text(&self) -> String {
        let grid = self.term.grid();
        let mut lines = Vec::with_capacity(grid.screen_lines());
        for row in 0..grid.screen_lines() {
            let line = Line(row as i32);
            let mut s = String::new();
            for cell in &grid[line] {
                s.push(cell.c);
            }
            lines.push(s.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// The full raw byte transcript seen so far (pre-terminal-emulation),
    /// lossily decoded, for grepping markers across scrollback that the
    /// visible screen has already scrolled past.
    pub fn raw_transcript(&self) -> String {
        String::from_utf8_lossy(&self.raw_log).into_owned()
    }

    /// The set of distinct substrings in the transcript so far matching
    /// `pattern` (a regex) -- see [`unique_matches`]. The standard way these
    /// tests prove a background refresh's output genuinely landed and
    /// changed content: count distinct `SLOW-<timestamp>`-style markers
    /// rather than just checking that *some* marker is present (which
    /// cached/replayed or unchanged output would also satisfy).
    pub fn distinct_markers(&self, pattern: &str) -> std::collections::BTreeSet<String> {
        unique_matches(&self.raw_transcript(), pattern)
    }

    /// Extract the payload following the last occurrence of `TAG:` in the
    /// transcript, up to the next newline. Searches for `TAG:` anywhere
    /// (not just at the start of a `.lines()`-split line): real terminal
    /// output frequently prefixes a `print()`'s output with escape sequences
    /// on the *same* raw line with no intervening `\n` (OSC title-updates, a
    /// bracketed-paste-mode-disable `ESC[?2004l`, etc.), which a
    /// start-of-line check would silently miss -- indistinguishable from
    /// "the probe never ran" if you're not looking at the raw bytes.
    pub fn extract_tag(&self, tag: &str) -> Option<String> {
        extract_tag(&self.raw_transcript(), tag)
    }

    pub fn pid(&self) -> u32 {
        self.pty.child().id()
    }

    /// Send SIGKILL to the child directly (bypassing normal shutdown), to
    /// exercise "shell killed mid-refresh" cleanup behavior.
    pub fn kill(&self) {
        let _ = Command::new("kill")
            .args(["-9", &self.pid().to_string()])
            .status();
    }

    /// Resize the pty's window size (in columns/lines), exactly like a real
    /// terminal emulator resizing its window. This issues the same
    /// `TIOCSWINSZ` ioctl a real terminal would, which the kernel translates
    /// into a `SIGWINCH` delivered to the foreground process group -- no
    /// manual signal-sending needed. Also resizes our own `Term` so the
    /// emulator's model of the screen stays consistent with the real pty.
    pub fn resize(&mut self, columns: usize, lines: usize) {
        let window_size = WindowSize {
            num_lines: lines as u16,
            num_cols: columns as u16,
            cell_width: 8,
            cell_height: 16,
        };
        self.pty.on_resize(window_size);
        let size = TermSize {
            columns,
            screen_lines: lines,
        };
        self.term.resize(size);
    }
}

/// The real, freshly built `starship` binary, built once and shared across
/// every test in this module (mirrors "Building starship (cargo build)" in
/// the original shell-script harness, just done once per test run instead
/// of once per script).
pub static STARSHIP_BIN: LazyLock<PathBuf> = LazyLock::new(build_starship_bin);

fn build_starship_bin() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new(env!("CARGO"))
        .args(["build", "--bin", "starship"])
        .current_dir(&manifest_dir)
        .status()
        .expect("failed to run `cargo build --bin starship`");
    assert!(status.success(), "cargo build --bin starship failed");

    // The test binary itself lives at target/<profile>/deps/<name>-<hash>;
    // the bin we just built is one directory up, at target/<profile>/starship.
    let test_exe = std::env::current_exe().expect("failed to resolve current test executable");
    let profile_dir = test_exe
        .parent() // deps/
        .and_then(Path::parent) // <profile>/
        .expect("unexpected test executable location");
    let bin = profile_dir.join("starship");
    assert!(
        bin.is_file(),
        "expected starship binary at {}, but it doesn't exist",
        bin.display()
    );
    bin
}

/// A scratch directory with an isolated `STARSHIP_CONFIG`/`STARSHIP_CACHE`,
/// containing a `starship.toml` with one `custom` module slow enough to
/// exceed `SLOW_MODULE_THRESHOLD` (5ms, see `src/modules/mod.rs`) -- so it
/// actually gets cached/replayed -- and one fast module, so both the
/// CacheRead and Refresh code paths are genuinely exercised end to end
/// against the real binary and the real on-disk cache. Shells that also need
/// an isolated `HOME`/`XDG_*` sandbox create their own subdirectory under
/// [`Self::dir`] for that, since the exact set of directories needed differs
/// per shell.
pub struct ScratchEnv {
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
    pub cache_path: PathBuf,
}

impl ScratchEnv {
    /// `slow_ms` is how long the `custom.slow` module's command sleeps for.
    /// Keep this comfortably above the 5ms threshold but well under typical
    /// per-check wait budgets (100-300ms is a good default).
    pub fn new(slow_ms: u64) -> std::io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("starship.toml");
        let cache_path = dir.path().join("cache");
        fs::create_dir_all(&cache_path)?;

        let slow_secs = slow_ms as f64 / 1000.0;
        let config = render_config(
            "${custom.slow}${custom.fast}$character",
            None,
            true,
            &[
                ("slow", custom_module(&format!("sleep {slow_secs} && echo SLOW"))),
                ("fast", custom_module("echo FAST")),
            ],
            Some(CharacterConfig {
                success_symbol: "[>](green)",
                ..Default::default()
            }),
        );
        fs::write(&config_path, config)?;

        Ok(Self { dir, config_path, cache_path })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Resolve a real GNU/BSD bash for the scratch config's `shell =` entries,
/// preferring `/bin/bash` (the system bash) over whatever `bash` resolves to
/// on `$PATH`, which on this machine can be a non-bash `bash`-compatible
/// shim that doesn't support `--noprofile`/`--norc`.
pub fn real_bash_path() -> &'static str {
    for candidate in ["/bin/bash", "/usr/bin/bash"] {
        if Path::new(candidate).is_file()
            && Command::new(candidate)
                .args(["--noprofile", "--norc", "-c", "true"])
                .status()
                .is_ok_and(|s| s.success())
        {
            return candidate;
        }
    }
    "bash"
}

pub use crate::configs::character::CharacterConfig;
pub use crate::configs::custom::CustomConfig;

/// Build a `[custom.<name>]` module config for a scratch `starship.toml`,
/// running `command` through a real, isolated bash (`--noprofile --norc`) so
/// results are deterministic regardless of the invoking user's shell config.
/// Returns the actual `CustomConfig` type starship itself deserializes
/// config into, rather than a hand-formatted TOML string, so a typo'd field
/// name is a compile error instead of a silently-ignored/misparsed key.
pub fn custom_module(command: &str) -> CustomConfig<'_> {
    CustomConfig {
        command,
        when: crate::config::Either::First(true),
        format: "[$output]($style) ",
        shell: crate::config::VecOr(vec![real_bash_path(), "--noprofile", "--norc"]),
        ..Default::default()
    }
}

/// The real root-level config schema (`format`/`right_format`/`add_newline`/
/// ...), re-exported so tests can build one with `..Default::default()`
/// rather than this harness re-declaring those field names as string keys.
pub use crate::configs::StarshipRootConfig;

/// A scratch `starship.toml`'s top-level shape, composed entirely from
/// starship's own real config structs (`StarshipRootConfig`, `CharacterConfig`,
/// `CustomConfig`) via their own `Serialize` impls -- not a hand-built,
/// stringly-keyed `toml::Table` -- so a field getting renamed in the real
/// schema is a compile error here, not a silently-ignored/misparsed key in a
/// generated file that happens to still parse.
#[derive(serde::Serialize)]
struct ScratchConfig<'a> {
    #[serde(flatten)]
    root: StarshipRootConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CharacterConfig<'a>>,
    custom: indexmap::IndexMap<&'a str, CustomConfig<'a>>,
}

/// Assemble a scratch `starship.toml` from typed module configs (see
/// [`custom_module`]).
pub fn render_config(
    format: &str,
    right_format: Option<&str>,
    add_newline: bool,
    modules: &[(&str, CustomConfig)],
    character: Option<CharacterConfig>,
) -> String {
    // `StarshipRootConfig` has a private `schema` field, so it can't be
    // built with struct-literal + `..Default::default()` from outside its
    // module -- construct via `::default()` then assign the public fields.
    let mut root = StarshipRootConfig::default();
    root.format = format.to_string();
    root.right_format = right_format.unwrap_or_default().to_string();
    root.add_newline = add_newline;
    let custom = modules.iter().map(|(name, config)| (*name, config.clone())).collect();
    let scratch = ScratchConfig { root, character, custom };
    toml::to_string(&scratch).expect("failed to serialize scratch config")
}

/// Read the given `src/init/starship.*` file and substitute the `::STARSHIP::`
/// template token for the real built binary's path, exactly like the actual
/// shell-integration install process does.
pub fn substituted_init_script(relative_path: &str) -> String {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = fs::read_to_string(manifest_dir.join(relative_path))
        .unwrap_or_else(|e| panic!("failed to read {relative_path}: {e}"));
    src.replace("::STARSHIP::", &STARSHIP_BIN.display().to_string())
}

/// PIDs of currently-running processes whose command line contains `pattern`
/// AND whose current working directory is inside `dir` -- the disambiguator
/// that makes a leak check safe to run alongside every other shell's test
/// file. This machine runs every other shell's test file concurrently by
/// default (`cargo test` parallelizes `#[test]` fns), and they all spawn
/// processes matching a bare pattern like "starship prompt" too -- confirmed
/// by direct observation to cause real false-positive "leak" failures when
/// run alongside the rest of the suite. Every test in this suite launches
/// its subject processes with a cwd inside its own scratch dir, so cwd
/// reliably distinguishes "this test's own (possibly leaked) processes" from
/// "some sibling test's legitimate, still-running background refresh" even
/// though both match the same command-line pattern. Survives reparenting on
/// an abrupt kill too (an orphaned process keeps its original cwd even after
/// its parent dies and it gets reparented to init/launchd), unlike scoping
/// by parent-pid chain.
pub fn pids_matching_with_cwd_under(pattern: &str, dir: &Path) -> Vec<u32> {
    let output = Command::new("ps").args(["-eo", "pid,command"]).output().expect("failed to run ps");
    let dir = dir.display().to_string();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(pattern) && !line.contains("grep"))
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .filter(|pid| {
            let cwd_out = Command::new("lsof").args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"]).output();
            cwd_out.is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| l.strip_prefix('n'))
                    .any(|cwd| cwd.starts_with(&dir))
            })
        })
        .collect()
}

/// Assert no process matching `pattern` with a cwd inside `dir` is currently
/// running -- see [`pids_matching_with_cwd_under`] for why this is the safe
/// way to check for leaks when other tests are running concurrently.
pub fn assert_no_orphaned_processes(pattern: &str, dir: &Path) {
    let pids = pids_matching_with_cwd_under(pattern, dir);
    assert!(pids.is_empty(), "orphaned process(es) matching {pattern:?} with cwd under {}: {pids:?}", dir.display());
}

/// Extract the payload following the last occurrence of `TAG:` in `text`, up
/// to the next newline. See [`PtySession::extract_tag`] for why this
/// searches anywhere in the text rather than requiring `TAG:` at the exact
/// start of a line.
pub fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let prefix = format!("{tag}:");
    let mut last = None;
    let mut from = 0;
    while let Some(rel) = text[from..].find(&prefix) {
        let start = from + rel + prefix.len();
        let rest = &text[start..];
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        last = Some(rest[..end].to_string());
        from = start;
    }
    last
}

/// Minimal ad hoc extraction of a flat JSON field's value from a
/// `{"field": value, ...}`-shaped payload (as printed by the Python/JSON
/// probes in `xonsh.rs`, or similar single-level dicts elsewhere): finds
/// `"field":` and takes the following value up to the next top-level `,` or
/// closing `}`, unquoting it if it's a JSON string. Good enough for the
/// flat, single-level dicts these probes print; not a general JSON parser.
pub fn json_field<'a>(payload: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\":");
    let start = payload.find(&needle)?;
    let rest = payload[start + needle.len()..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim())
    }
}

/// Serializes tests that assert on system-wide process counts
/// (`count_processes_matching`) so they don't observe each other's
/// in-flight child processes when run concurrently by the default
/// multi-threaded test runner.
pub static PROCESS_COUNT_LOCK: Mutex<()> = Mutex::new(());

/// The set of distinct substrings in `text` matching `pattern` (a regex).
/// Used throughout these tests to prove a background refresh's output
/// genuinely landed and changed content -- e.g. counting distinct
/// `SLOW-<timestamp>` markers across a pty transcript -- rather than just
/// checking that *some* marker is present (which cached/replayed or
/// unchanged output would also satisfy).
pub fn unique_matches(text: &str, pattern: &str) -> std::collections::BTreeSet<String> {
    let re = regex::Regex::new(pattern).expect("invalid regex");
    re.find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}
