set-env STARSHIP_SHELL "elvish"
set-env STARSHIP_SESSION_KEY (to-string (randint 10000000000000 10000000000000000))

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

# Async prompt (opt out with STARSHIP_ASYNC=0): the prompt closures below do
# instant --fast paints (slow modules served from the on-disk cache), and
# after each command one background `starship refresh` recomputes
# both prompts and rewrites that cache; `edit:redraw &full` then re-evaluates
# the closures so they pick up the fresh values.
#
# $-starship-paint splices to --fast when async is on, to nothing when off.
var -starship-paint = []

if (not-eq $E:STARSHIP_ASYNC '0') {
    set -starship-paint = [--fast]

    # Suppress the "job ... finished" notification for background refreshes.
    # This must be set globally (not via `tmp` inside the hook below): `tmp`
    # restores its old value when the *enclosing function* returns, which
    # happens immediately after backgrounding -starship-defer-job, long
    # before that job actually finishes and the notification would fire.
    set notify-bg-job-success = $false

    # Named function (rather than an inline `{ ... }` literal) so that if a
    # notification ever is printed, Elvish shows a short `job <name> &
    # finished` line instead of dumping this function's entire source text.
    #
    # Elvish has no supported way for a script to obtain the PID/job handle of
    # a command it backgrounds with `&`, so an in-flight refresh is never
    # cancelled -- it's left to finish on its own. That is safe: the cache is
    # written atomically (temp file + rename), so an overlapping refresh can
    # only be superseded by a newer one, never corrupt the cache. The poke
    # line `refresh` prints is discarded; the redraw below is the repaint
    # signal here.
    fn -starship-defer-job {|jobs-count cmd-duration status-code logical-path|
        ::STARSHIP:: refresh --jobs=$jobs-count --cmd-duration=$cmd-duration --status=$status-code --logical-path=$logical-path >/dev/null
        edit:redraw &full=$true
    }

    fn -starship-defer {|m|
        # Snapshot *all* relevant args accurately at command completion time.
        var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
        -starship-defer-job $num-bg-jobs $cmd-duration $cmd-status-code $pwd &
    }

    set edit:after-command = [ $@edit:after-command $-starship-defer~ ]
}

# Install starship
set edit:prompt = {
    var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
    ::STARSHIP:: prompt $@-starship-paint --jobs=$num-bg-jobs --cmd-duration=$cmd-duration --status=$cmd-status-code --logical-path=$pwd
}

set edit:rprompt = {
    var cmd-duration = (printf "%.0f" (* $edit:command-duration 1000))
    ::STARSHIP:: prompt $@-starship-paint --right --jobs=$num-bg-jobs --cmd-duration=$cmd-duration --status=$cmd-status-code --logical-path=$pwd
}

# Live-update tick (root `refresh_interval`): not wired for Elvish, so the
# refresh above runs without `--watch`. A periodic repaint would
# need the prompt *content* to be recomputed while the editor sits idle, but
# Elvish evaluates the prompt closures once per edit cycle and `edit:redraw
# &full` reuses that result mid-cycle (verified: the clock stays frozen until
# the next command). Elvish exposes no timer/hook that recomputes the prompt
# during an idle read, so live updates are a no-op here.
