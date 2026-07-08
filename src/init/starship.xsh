import uuid
import threading


# Enable Starship's async/cache mode by default so that plain `prompt` calls are
# treated as fast "cache read" paints and `--async` calls perform full refreshes +
# cache writes. Only default when unset -- respect an explicit STARSHIP_ASYNC=0
# (or any falsy value) set by the user as an opt-out that falls back to the old,
# fully-synchronous single-call-per-prompt behavior.
$STARSHIP_ASYNC = ${...}.get('STARSHIP_ASYNC', '1')
_STARSHIP_ASYNC_ENABLED = $STARSHIP_ASYNC not in ('0', '', None)

# Thread-safe storage for the latest fully rendered prompt segments.
# The prompt functions return these instantly; background threads + invalidate()
# keep them up to date without blocking prompt rendering.
_STARSHIP_LEFT = ''
_STARSHIP_RIGHT = ''
_STARSHIP_LOCK = threading.Lock()


def _starship_jobs():
    # I believe this is equivalent to xonsh.jobs.get_next_job_number() for our purposes,
    # but we can't use that function because of https://gitter.im/xonsh/xonsh?at=60e8832d82dd9050f5e0c96a
    return sum(1 for job in __xonsh__.all_jobs.values() if job['obj'] and job['obj'].poll() is None)


def _starship_cmd_info():
    """Extract status, jobs and cmd duration from history for the *previous* command."""
    last_cmd = __xonsh__.history[-1] if __xonsh__.history else None
    status = last_cmd.rtn if last_cmd else 0
    jobs = _starship_jobs()
    duration = round((last_cmd.ts[1] - last_cmd.ts[0]) * 1000) if last_cmd else 0
    return status, jobs, duration


def _starship_refresh(right=False, use_async=True):
    """Invoke Starship and return the rendered prompt (str).
    use_async=True  -> appends --async so Starship runs in ExecMode::Refresh (full compute + cache write)
    use_async=False -> plain call; when $STARSHIP_ASYNC=1 this becomes a fast CacheRead paint.
    """
    status, jobs, duration = _starship_cmd_info()
    args = [
        "::STARSHIP::",
        "prompt",
        f"--status={status}",
        f"--jobs={jobs}",
        f"--cmd-duration={duration}",
    ]
    if right:
        args.append("--right")
    if use_async:
        args.append("--async")
    try:
        out = __xonsh__.subproc_captured_stdout(args)
        return out
    except Exception:
        return ''


def _starship_invalidate():
    """Thread-safe request to prompt_toolkit to redraw the prompt.
    This is the key to showing the updated (full) prompt after the background
    thread finishes, without the user having to press a key.
    """
    try:
        # Xonsh ptk shell nests as: __xonsh__.shell.shell.prompter.app
        prompter = __xonsh__.shell.shell.prompter
        app = getattr(prompter, 'app', None)
        if app is not None:
            app.invalidate()
    except Exception:
        # readline backend, early startup, or no active app: safe to ignore
        pass


def _starship_worker():
    """Background worker: compute fresh left + right prompts and publish them."""
    try:
        left = _starship_refresh(right=False, use_async=True)
        right = _starship_refresh(right=True, use_async=True)
        global _STARSHIP_LEFT, _STARSHIP_RIGHT
        with _STARSHIP_LOCK:
            _STARSHIP_LEFT = left
            _STARSHIP_RIGHT = right
        _starship_invalidate()
    except Exception:
        # Never let background errors affect the shell or future prompts
        pass


def _starship_on_pre_prompt(**kwargs):
    """Hook that triggers an async refresh for the *next* prompt display.
    Started on every pre-prompt so that slow modules (git status in huge repos etc.)
    are computed off the UI thread.
    """
    # Fire-and-forget daemon thread. Multiple overlapping threads are harmless;
    # the last one to finish wins and will call invalidate().
    t = threading.Thread(target=_starship_worker, daemon=True)
    t.start()


if _STARSHIP_ASYNC_ENABLED:
    events.on_pre_prompt(_starship_on_pre_prompt)


def starship_prompt():
    """$PROMPT function. Returns the last known rendered left prompt instantly.
    Falls back to a (fast) synchronous call only on the very first prompt.
    When async mode is disabled (STARSHIP_ASYNC=0), always performs the classic
    fully-synchronous single call per prompt -- no background thread involved.
    """
    if _STARSHIP_ASYNC_ENABLED:
        with _STARSHIP_LOCK:
            if _STARSHIP_LEFT:
                return _STARSHIP_LEFT
    # First paint, no value yet, or async disabled: perform a direct call
    # (CacheRead if STARSHIP_ASYNC is enabled, plain Direct render otherwise).
    return _starship_refresh(right=False, use_async=False)


def starship_rprompt():
    """$RIGHT_PROMPT function. Same semantics as starship_prompt for the right side."""
    if _STARSHIP_ASYNC_ENABLED:
        with _STARSHIP_LOCK:
            if _STARSHIP_RIGHT:
                return _STARSHIP_RIGHT
    return _starship_refresh(right=True, use_async=False)


if _STARSHIP_ASYNC_ENABLED:
    # Initial synchronous paint so the *very first* prompt the user sees is never
    # empty. Uses a non-async call so it is treated as the fast path.
    try:
        _STARSHIP_LEFT = _starship_refresh(right=False, use_async=False)
        _STARSHIP_RIGHT = _starship_refresh(right=True, use_async=False)
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
