import subprocess
import threading
import time
import uuid


# Async prompt (opt out with STARSHIP_ASYNC=0):
#   * $PROMPT/$RIGHT_PROMPT return the last published render instantly.
#   * After each prompt, one background `starship prompt --deferred --watch`
#     recomputes both prompts and rewrites the on-disk cache, then prints one
#     line per repaint this session should do: one when the refresh lands,
#     then one per configured `refresh_interval` tick so dynamic modules
#     (e.g. `time`) keep advancing while the prompt sits idle. A reader
#     thread turns each line into fresh --cached paints (slow modules served
#     from the cache) plus a prompt_toolkit invalidate().
_STARSHIP_ASYNC = ${...}.get('STARSHIP_ASYNC', '1') not in ('0', '', None)

# Thread-safe storage for the latest fully rendered prompt segments.
_STARSHIP_LEFT = ''
_STARSHIP_RIGHT = ''
_STARSHIP_LOCK = threading.Lock()
_STARSHIP_PROC = None


def _starship_jobs():
    # I believe this is equivalent to xonsh.jobs.get_next_job_number() for our purposes,
    # but we can't use that function because of https://gitter.im/xonsh/xonsh?at=60e8832d82dd9050f5e0c96a
    return sum(1 for job in __xonsh__.all_jobs.values() if job['obj'] and job['obj'].poll() is None)


def _starship_args(extra):
    """Starship argv with status, jobs and duration of the *previous* command."""
    last_cmd = __xonsh__.history[-1] if __xonsh__.history else None
    status = last_cmd.rtn if last_cmd else 0
    duration = round((last_cmd.ts[1] - last_cmd.ts[0]) * 1000) if last_cmd else 0
    return [
        "::STARSHIP::",
        "prompt",
        f"--status={status}",
        f"--jobs={_starship_jobs()}",
        f"--cmd-duration={duration}",
        *extra,
    ]


def _starship_paint(right=False):
    """Render a prompt synchronously. With async on this is a fast --cached
    paint; otherwise it is the classic direct render. Runs plain subprocess
    (with xonsh's exported env) so it is safe from any thread."""
    extra = ["--cached"] if _STARSHIP_ASYNC else []
    if right:
        extra.append("--right")
    try:
        return subprocess.run(
            _starship_args(extra),
            capture_output=True, text=True, env=__xonsh__.env.detype(),
        ).stdout
    except Exception:
        return ''


def _starship_publish():
    """Recompute both paints, publish them, and ask prompt_toolkit to redraw.
    invalidate() is prompt_toolkit's thread-safe redraw request; it no-ops
    when no app is active (e.g. while a command is running)."""
    global _STARSHIP_LEFT, _STARSHIP_RIGHT
    left = _starship_paint()
    right = _starship_paint(right=True)
    with _STARSHIP_LOCK:
        _STARSHIP_LEFT = left
        _STARSHIP_RIGHT = right
    try:
        # Xonsh ptk shell nests as: __xonsh__.shell.shell.prompter.app
        prompter = __xonsh__.shell.shell.prompter
        app = getattr(prompter, 'app', None)
        if app is not None:
            app.invalidate()
    except Exception:
        # readline backend, early startup, or no active app: safe to ignore
        pass


def _starship_reader(proc):
    """One poke line from the watcher = one repaint (refresh landed or a
    live-update tick elapsed). Ends when the watcher exits.

    Uses an explicit `readline()` loop rather than `for line in proc.stdout`:
    confirmed by direct testing that plain iteration over a `Popen` pipe does
    not reliably yield each line promptly when the writer trickles output
    slowly (one poke per `refresh_interval` second here) -- the iterator's
    own internal read-ahead buffering can withhold an already-available line
    until more data arrives, silently stalling the tick. `bufsize=1` (below,
    at the Popen call) plus `readline()` is the standard fix for this exact
    class of "slow producer" pipe-reading bug.
    """
    try:
        for _ in iter(proc.stdout.readline, ''):
            _starship_publish()
    except Exception:
        # Never let background errors affect the shell or future prompts.
        pass


# Minimum real time between two watcher (re)launches. xonsh's own
# prompt-cycle bookkeeping fires `on_pre_prompt` several times in quick
# succession around a single logical input line -- not just once per actual
# "one command, one prompt" cycle (confirmed by direct experimentation; see
# `resize_does_not_refire_on_pre_prompt` in the test suite for this file).
# `len(__xonsh__.history)` is NOT a reliable guard against this: it was
# previously used on the theory that these extra firings were "spurious" and
# would show an unchanged history length, but direct tracing showed the
# opposite -- history length genuinely increments on each of these rapid
# firings too (e.g. once per statement in a pasted/compound input), so that
# guard let every one of them through. Each pass-through kills and relaunches
# the watcher, and the resulting overlapping `--deferred` processes contend
# for CPU; that contention was observed to make otherwise-fast, never-cached
# modules (e.g. `time`) occasionally cross the async refresh's slow-module
# threshold, permanently freezing them in the cache until the next full
# refresh (see `src/cache.rs`, `SLOW_MODULE_THRESHOLD` in
# `src/modules/mod.rs`) -- the frozen-clock bug this guards against.
#
# A wall-clock debounce sidesteps the unreliable history-length signal
# entirely: real, separate command prompts are always at least this far
# apart in interactive use, while a burst of same-instant refires collapses
# into a single relaunch.
_STARSHIP_DEBOUNCE_SECONDS = 0.2
_STARSHIP_LAST_LAUNCH = 0.0


def _starship_on_pre_prompt(**kwargs):
    """Replace any in-flight watcher with a fresh one carrying the finished
    command's context -- but only when enough wall-clock time has passed
    since the last (re)launch, so a burst of same-instant `on_pre_prompt`
    refires collapses into a single relaunch instead of repeatedly killing
    and restarting the watcher (see module docstring above for why that
    matters: overlapping watchers contend for CPU and can freeze fast
    modules stale).
    """
    global _STARSHIP_PROC, _STARSHIP_LAST_LAUNCH
    now = time.monotonic()
    if _STARSHIP_PROC is not None and (now - _STARSHIP_LAST_LAUNCH) < _STARSHIP_DEBOUNCE_SECONDS:
        return
    _STARSHIP_LAST_LAUNCH = now

    if _STARSHIP_PROC is not None:
        try:
            _STARSHIP_PROC.terminate()
        except Exception:
            pass
    try:
        _STARSHIP_PROC = subprocess.Popen(
            _starship_args(["--deferred", "--watch"]),
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            bufsize=1,
            env=__xonsh__.env.detype(),
        )
    except Exception:
        _STARSHIP_PROC = None
        return
    threading.Thread(target=_starship_reader, args=(_STARSHIP_PROC,), daemon=True).start()


def starship_prompt():
    """$PROMPT function. Returns the last published left prompt instantly.
    Falls back to a synchronous paint on the very first prompt, or always
    when async is disabled."""
    if _STARSHIP_ASYNC:
        with _STARSHIP_LOCK:
            if _STARSHIP_LEFT:
                return _STARSHIP_LEFT
    return _starship_paint()


def starship_rprompt():
    """$RIGHT_PROMPT function. Same semantics as starship_prompt."""
    if _STARSHIP_ASYNC:
        with _STARSHIP_LOCK:
            if _STARSHIP_RIGHT:
                return _STARSHIP_RIGHT
    return _starship_paint(right=True)


if _STARSHIP_ASYNC:
    events.on_pre_prompt(_starship_on_pre_prompt)
    # Initial synchronous paint so the *very first* prompt is never empty.
    try:
        _STARSHIP_LEFT = _starship_paint()
        _STARSHIP_RIGHT = _starship_paint(right=True)
    except Exception:
        pass

# Integration note for $ENABLE_ASYNC_PROMPT:
# When $ENABLE_ASYNC_PROMPT is true, Xonsh itself renders $PROMPT_FIELDS entries
# using threads and updates them asynchronously. Our implementation uses an
# explicit on_pre_prompt + threading.Lock + prompt_toolkit invalidate path
# that works independently of (and can coexist with) Xonsh's mechanism because
# we drive the whole $PROMPT/$RIGHT_PROMPT directly. If users prefer the native
# field-based async they can use the separate xontrib-prompt-starship instead.

$PROMPT = starship_prompt
$RIGHT_PROMPT = starship_rprompt
$STARSHIP_SHELL = "xonsh"
$STARSHIP_SESSION_KEY = uuid.uuid4().hex
