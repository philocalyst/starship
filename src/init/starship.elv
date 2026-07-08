set-env STARSHIP_SHELL "elvish"
set-env STARSHIP_SESSION_KEY (to-string (randint 10000000000000 10000000000000000))

# Opt-in to async prompt repainting (default on, like Zsh).
# When enabled:
#   * Plain starship prompt      => fast CacheRead (slow work from cache/omitted)
#   * starship prompt --async    => background Refresh (full compute + cache write)
# Prompt fns snapshot args + do fast paint, schedule bg refresh; edit:redraw &full
# forces Elvish to re-evaluate the prompt closures (now seeing fresh values).
if (eq $E:STARSHIP_ASYNC '') { set-env STARSHIP_ASYNC 1 }

# Define Hooks
var cmd-status-code = 0

fn starship-after-command-hook {|m|
    var error = $m[error]
    if (is $error $nil) {
        set cmd-status-code = 0
    } else {
        try {
            set cmd-status-code = $error[reason][exit-status]
        } catch {
            # The error is from the built-in commands and they have no status code.
            set cmd-status-code = 1
        }
    }
}

# Install Hooks
set edit:after-command = [ $@edit:after-command $starship-after-command-hook~ ]

# -------------------------------------------------------------------
# Full async prompt support.
# - edit:redraw &full=$true for complete repaint (left + right prompt).
# - Snapshot of *all* args (duration, jobs, status, path) taken in after-command
#   (accurate $edit:command-duration for the finished command).
# - Right prompt supported: same cache warming serves --right calls; redraw
#   updates both sides. Direct rprompt calls also benefit from CacheRead.
# - STARSHIP_ASYNC enables CacheRead (plain calls in prompt fns) / Refresh (--async).
#
# Note: Elvish has no supported way for a script to obtain the PID/job handle
# of a command it backgrounds with `&` (`{ ... } &` runs as a goroutine within
# the same Elvish process and returns no value), and the `jobs` builtin is not
# an interactive job-table introspection facility that can be parsed to find
# it after the fact. So an in-flight refresh is not cancelled when a new
# command completes; it's simply left to finish on its own. This is safe
# because cache.rs writes are atomic (tmp file + rename) per cache key, so an
# overlapping refresh can only ever be superseded by a newer one, never
# corrupt the cache.
# -------------------------------------------------------------------

if (not-eq $E:STARSHIP_ASYNC '0') {
    # Suppress the "job ... finished" notification for background refreshes.
    # This must be set globally (not via `tmp` inside the hook below): `tmp`
    # restores its old value when the *enclosing function* returns, which
    # happens immediately after backgrounding -starship-async-refresh-job,
    # long before that job actually finishes and the notification would fire.
    set notify-bg-job-success = $false

    # Named function (rather than an inline `{ ... }` literal) so that if a
    # notification ever is printed, Elvish shows a short `job <name> &
    # finished` line instead of dumping this function's entire source text.
    fn -starship-async-refresh-job {|jobs-count cmd-duration status-code logical-path|
        # Background Refresh: full recompute + cache write. Output discarded;
        # the side-effect is the updated cache for subsequent plain calls.
        ::STARSHIP:: prompt --async --jobs=$jobs-count --cmd-duration=$cmd-duration --status=$status-code --logical-path=$logical-path >/dev/null
        # Full redraw so that both left prompt and rprompt are repainted
        # using fresh CacheRead results (slow modules now populated).
        edit:redraw &full=$true
    }

    fn starship-async-refresh {|m|
        # Snapshot *all* relevant args accurately at command completion time.
        var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
        var status-code = $cmd-status-code
        var jobs-count = $num-bg-jobs
        var logical-path = $pwd

        -starship-async-refresh-job $jobs-count $cmd-duration $status-code $logical-path &
    }

    set edit:after-command = [ $@edit:after-command $starship-async-refresh~ ]
}

# Install starship.
# Plain calls here become CacheRead (fast) when STARSHIP_ASYNC != 0.
# Right prompt is fully supported and receives the same cache benefits;
# redraw &full above ensures rprompt is also updated.
set edit:prompt = {
    var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
    ::STARSHIP:: prompt --jobs=$num-bg-jobs --cmd-duration=$cmd-duration --status=$cmd-status-code --logical-path=$pwd
}

set edit:rprompt = {
    var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
    ::STARSHIP:: prompt --right --jobs=$num-bg-jobs --cmd-duration=$cmd-duration --status=$cmd-status-code --logical-path=$pwd
}
