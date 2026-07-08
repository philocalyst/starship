//! STATIC-ONLY regression test for `src/init/starship.lua` (the CMD.exe/Clink
//! integration).
//!
//! WHY STATIC-ONLY: this file only executes inside real Clink running under
//! Windows CMD.exe (it depends on `clink.promptfilter`, `clink.promptcoroutine`,
//! `clink.onbeginedit`/`onendedit`, `io.popenyield`, `rl.getvariable`,
//! `console.getwidth` -- none of which exist outside Clink's embedded Lua host).
//! There is no Windows/Clink environment available in CI or on this machine,
//! so a genuine behavioral/integration test is not possible here. This module
//! instead does the strongest checks available without that runtime:
//!   1. Lua syntax validity (via `luac -p`), after substituting the
//!      `::STARSHIP::` template token for a dummy string, since that token is
//!      not valid standalone Lua and is normally substituted by the starship
//!      installer before the file is ever loaded by Clink.
//!   2. Structural/regex-based assertions that pin down specific facts that
//!      were verified by hand against Clink's real documented Lua API
//!      (<https://chrisant996.github.io/clink/clink.html>). These exist so
//!      that a careless future edit which silently breaks one of these
//!      documented-API assumptions gets caught mechanically, even though we
//!      can't execute the file for real.
//!
//! This module does NOT prove the async prompt feature behaves correctly at
//! runtime. It only proves the file hasn't regressed on the specific points
//! checked below since they were last verified against the docs.
//!
//! Ported from the original `tests/init-async/test_lua_static.sh` shell
//! script, preserving every check and its rationale.

use std::path::PathBuf;
use std::process::Command;

/// Read `src/init/starship.lua`'s raw source (relative to the crate root).
fn read_target() -> String {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = manifest_dir.join("src/init/starship.lua");
    std::fs::read_to_string(&target)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", target.display()))
}

/// Substitute the `::STARSHIP::` template token for a dummy filesystem path,
/// the same way the real starship installer substitutes it before Clink ever
/// loads this file. `::STARSHIP::` appears inside an existing Lua string
/// literal (e.g. `"::STARSHIP:: " .. cmd`), where the real installer
/// substitutes it with a raw filesystem path to the starship binary -- NOT a
/// separately-quoted Lua expression. We substitute a bare dummy path (no
/// added quotes) so the surrounding string literal stays well-formed.
fn substitute_starship_token(src: &str) -> String {
    src.replace("::STARSHIP::", "C:/dummy/starship.exe")
}

/// Locate a `luac` binary on PATH, trying versioned names in the same order
/// as the original shell script. Returns `None` if none is found.
fn find_luac() -> Option<String> {
    for candidate in ["luac", "luac5.4", "luac5.3", "luac5.1"] {
        if Command::new("sh")
            .args(["-c", &format!("command -v {candidate}")])
            .output()
            .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Attempt a small, reversible `brew install lua` if no `luac` is present and
/// Homebrew is available, mirroring the original script's fallback. Returns
/// the resolved `luac` binary name if installation succeeded and a binary
/// was subsequently found.
fn try_install_lua_via_brew() -> Option<String> {
    let brew_available = Command::new("sh")
        .args(["-c", "command -v brew"])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    if !brew_available {
        return None;
    }

    eprintln!(
        "No luac found; installing 'lua' via Homebrew (small, reversible: 'brew uninstall lua' to remove)..."
    );
    let install_ok = Command::new("brew")
        .args(["install", "lua"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !install_ok {
        return None;
    }

    find_luac()
}

/// Check 1: Lua syntax validity via `luac -p`, with `::STARSHIP::`
/// substituted for a dummy string first. If no Lua toolchain is available
/// (and Homebrew install is unavailable/fails), skip gracefully rather than
/// failing the whole test -- matching the original script's behavior of
/// reporting a clear FAIL-with-explanation for this one check while other
/// checks still run. Here we degrade to a printed skip message instead,
/// since the structural checks below are independent of this one and do not
/// require a Lua toolchain.
#[test]
fn syntax_check_via_luac() {
    let luac = find_luac().or_else(try_install_lua_via_brew);

    let Some(luac_bin) = luac else {
        eprintln!(
            "SKIP: no Lua toolchain (luac) available and Homebrew install failed/unavailable; cannot run syntax check"
        );
        return;
    };

    let raw = read_target();
    let substituted = substitute_starship_token(&raw);

    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!(
        "starship_lua_static_{}_{}.lua",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp_path, &substituted).expect("failed to write temp lua file");

    let output = Command::new(&luac_bin)
        .args(["-p", tmp_path.to_str().unwrap()])
        .output();

    let _ = std::fs::remove_file(&tmp_path);

    match output {
        Ok(out) => {
            assert!(
                out.status.success(),
                "luac -p syntax check failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Err(e) => panic!("failed to execute {luac_bin}: {e}"),
    }
}

/// Check 2.1: the `10070000` version-encoding magic number must still be
/// present and must still correspond to v1.7.0 under Clink's Mmmmpppp
/// encoding (major*10^7 + minor*10^4 + patch). Verified:
/// 1*10000000 + 7*10000 + 0 = 10070000. This gates the "cookie" parameter to
/// `clink.promptcoroutine`, documented as added in Clink v1.7.0.
#[test]
fn version_gate_10070000_present_and_correct() {
    let src = read_target();
    assert!(
        src.contains("10070000"),
        "10070000 (v1.7.0 cookie-support version gate) not found in starship.lua -- has it been changed or removed?"
    );

    let computed = 10_i64.pow(7) + 7 * 10_i64.pow(4);
    assert_eq!(
        computed, 10070000,
        "10070000 arithmetic sanity check failed unexpectedly (got {computed})"
    );
}

/// Check 2.2: the pre-existing v1.2.30 minimum-version gate (`10020030`) at
/// the top of the file must be untouched.
#[test]
fn minimum_version_gate_10020030_untouched() {
    let src = read_target();
    let re = regex::Regex::new(r"(?m)^\s*if \(clink\.version_encoded or 0\) < 10020030 then")
        .unwrap();
    assert!(
        re.is_match(&src),
        "10020030 minimum-version check at top of file appears to have been changed or removed"
    );
}

/// Check 2.3: `io.popenyield`'s two-return-value handling (file, function)
/// must still exist: both the `pclose`-is-a-function branch and the
/// `f:close()` fallback. Confirmed from docs: in v1.3.31+, `io.popenyield`
/// may return a second value which is a function; if so it must be used
/// INSTEAD OF `file:close()`. This is real (not dead code) -- don't let it
/// be "simplified" away.
#[test]
fn popenyield_two_return_value_handling_present() {
    let src = read_target();

    let capture_re = regex::Regex::new(r"local f,\s*pclose\s*=\s*popen_fn\(").unwrap();
    assert!(
        capture_re.is_match(&src),
        "expected 'local f, pclose = popen_fn(...)' pattern not found -- has the two-return-value handling been removed?"
    );

    let branch_re = regex::Regex::new(r#"pclose\s+and\s+type\(pclose\)\s*==\s*"function""#).unwrap();
    assert!(
        branch_re.is_match(&src),
        "pclose-as-function branch not found -- this handles a real, documented io.popenyield return value; do not remove it"
    );

    let fallback_re = regex::Regex::new(r"(^|\s)f:close\(\)").unwrap();
    assert!(
        fallback_re.is_match(&src),
        "f:close() fallback not found -- required for io.popen and older Clink where popenyield returns only one value"
    );
}

/// Check 2.4: call-graph check. `run_starship(<right>, true)` -- i.e. calls
/// where the second (async) argument is literally `true` -- must appear ONLY
/// inside a closure passed to `get_async_prompt` (which forwards it to
/// `clink.promptcoroutine`). It must never be called with `async=true` at
/// the top level of `:filter()`/`:rightfilter()`. This matters because
/// `io.popenyield` is only valid to call from inside a coroutine; Clink only
/// provides that coroutine context via `clink.promptcoroutine`'s `func`
/// argument.
///
/// Approach: find every line calling `run_starship(..., true)`, then verify
/// it sits inside a `get_async_prompt(function() ... end, "<cookie>")` block
/// by checking the preceding lines (up to 5 above) open such a block.
#[test]
fn run_starship_async_true_only_inside_get_async_prompt_closure() {
    let src = read_target();
    let lines: Vec<&str> = src.lines().collect();

    let call_re = regex::Regex::new(r"run_starship\([a-z]*,\s*true\)").unwrap();
    let async_true_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| call_re.is_match(line))
        .map(|(i, _)| i + 1) // 1-indexed, matching grep -n
        .collect();

    assert!(
        !async_true_lines.is_empty(),
        "no 'run_starship(<right>, true)' call sites found -- expected exactly the async closures passed to get_async_prompt"
    );

    let opener_re = regex::Regex::new(r"get_async_prompt\(function\(\)").unwrap();
    let mut all_nested = true;
    for &ln in &async_true_lines {
        let start = ln.saturating_sub(5).max(1);
        let context = lines[(start - 1)..ln].join("\n");
        if !opener_re.is_match(&context) {
            all_nested = false;
            eprintln!(
                "line {ln}: 'run_starship(<right>, true)' not clearly nested inside a get_async_prompt(function() ... closure"
            );
        }
    }

    // Also confirm no call sites exist directly inside :filter()/:rightfilter()
    // bodies at statement level (i.e. not as the get_async_prompt closure body)
    // such as "local prompt_str = run_starship(..., true)" or similar direct
    // top-level assignment with async=true.
    let direct_assign_re = regex::Regex::new(r"prompt_str\s*=\s*run_starship\([a-z]*,\s*true\)").unwrap();
    if direct_assign_re.is_match(&src) {
        all_nested = false;
        eprintln!(
            "found a direct top-level assignment of run_starship(<right>, true) to prompt_str -- this would call io.popenyield outside a coroutine"
        );
    }

    assert!(
        all_nested,
        "at least one run_starship(<right>, true) call site is not safely nested inside a get_async_prompt closure -- see stderr output above"
    );

    // Sanity: exactly two such call sites are expected (left + right prompts).
    assert_eq!(
        async_true_lines.len(),
        2,
        "expected exactly 2 run_starship(<right>, true) call sites (left + right), found {}",
        async_true_lines.len()
    );
}

/// Check 2.5: `get_async_prompt` itself only ever forwards to
/// `clink.promptcoroutine` (never calls the `func` argument directly itself,
/// which would defeat the coroutine wrapping).
#[test]
fn get_async_prompt_forwards_to_clink_promptcoroutine() {
    let src = read_target();
    assert!(
        src.contains("clink.promptcoroutine(func"),
        "get_async_prompt no longer appears to forward its func argument to clink.promptcoroutine"
    );
}
