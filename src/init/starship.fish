function __starship_set_job_count --description 'Set STARSHIP_JOBS using fish job groups (or legacy PIDs if toggled)'
    # To force legacy behavior (process PIDs), set this variable to "false":
    #   set -g __starship_fish_use_job_groups "false"
    if test "$__starship_fish_use_job_groups" = "false"
        # Legacy behavior: counts PIDs (may overcount pipelines with terminated producers)
        set -g STARSHIP_JOBS (jobs -p 2>/dev/null | count)
    else
        # Default behavior: count job groups
        set -g STARSHIP_JOBS (jobs -g 2>/dev/null | count)
    end    
end

function __starship_defer --description 'Launch the background refresh/live-update watcher'
    # One background `starship prompt --deferred --watch`: it recomputes both
    # prompts, rewrites the on-disk cache the --cached paints read, then
    # prints one line per repaint this session should do -- one when the
    # refresh lands, then one per configured `refresh_interval` tick so
    # dynamic modules (e.g. `time`) keep advancing while the prompt sits
    # idle. Each line twiddles a per-session universal variable; the
    # --on-variable handler repaints, re-running fish_prompt's --cached paint.
    #
    # The variable is scoped by this session's PID (see $__starship_defer_var
    # below): universal variables are shared by every fish process on the
    # machine, so an unscoped name would repaint every unrelated session/tab.
    #
    # IMPORTANT: this must be a genuine external `fish -c` process (context
    # passed via `env`), NOT an inline `begin ... | while ...; end &` in this
    # shell. Confirmed by direct testing: backgrounding a `begin...end` block
    # that contains a pipe does not let fish kill the whole unit -- `kill` on
    # its reported pid is a no-op, so with `refresh_interval > 0` the ticking
    # `starship --deferred --watch` process (and the `while read` consuming
    # it) leaks forever, once per command run, and is never reaped. A plain
    # external process backgrounded with `&` does not have this problem:
    # killing it actually terminates it, which closes its read end of the
    # pipe, which gives the still-running `starship` child an EPIPE on its
    # next poke -- so it exits on its own within one refresh_interval tick.
    __starship_defer_kill
    # Resolve the currently-running fish's own absolute path rather than
    # relying on a bare `fish` resolving via $PATH: confirmed by a real user
    # report that this is not a given (e.g. `nix run nixpkgs#fish` does not
    # necessarily put `fish` itself on $PATH for child processes), which
    # silently prevented the watcher from ever starting -- no error surfaced
    # to the user, the cache just never updated. $__fish_bin_dir is a
    # long-standing internal fish variable pointing at the directory
    # containing the exact fish binary currently running, so this works
    # regardless of $PATH state.
    env \
        COLUMNS="$COLUMNS" \
        STARSHIP_CMD_STATUS="$STARSHIP_CMD_STATUS" \
        STARSHIP_CMD_PIPESTATUS="$STARSHIP_CMD_PIPESTATUS" \
        STARSHIP_KEYMAP="$STARSHIP_KEYMAP" \
        STARSHIP_DURATION="$STARSHIP_DURATION" \
        STARSHIP_JOBS="$STARSHIP_JOBS" \
        STARSHIP_DEFER_VAR="$__starship_defer_var" \
        "$__fish_bin_dir/fish" -c '
            ::STARSHIP:: prompt --deferred --watch \
                --terminal-width="$COLUMNS" \
                --status="$STARSHIP_CMD_STATUS" \
                --pipestatus="$STARSHIP_CMD_PIPESTATUS" \
                --keymap="$STARSHIP_KEYMAP" \
                --cmd-duration="$STARSHIP_DURATION" \
                --jobs="$STARSHIP_JOBS" 2>/dev/null | while read -l __starship_poke
                set -U $STARSHIP_DEFER_VAR (random)(random)
            end
        ' &
    set -g __starship_defer_pid $last_pid
    disown $last_pid 2>/dev/null
end

function __starship_defer_kill
    if set -q __starship_defer_pid
        kill $__starship_defer_pid 2>/dev/null
        set -e __starship_defer_pid
    end
end

function fish_prompt
    switch "$fish_key_bindings"
        case fish_hybrid_key_bindings fish_vi_key_bindings fish_helix_key_bindings
            set STARSHIP_KEYMAP "$fish_bind_mode"
        case '*'
            set STARSHIP_KEYMAP insert
    end

    set STARSHIP_CMD_PIPESTATUS $pipestatus
    set STARSHIP_CMD_STATUS $status
    # Account for changes in variable name between v2.7 and v3.0
    set STARSHIP_DURATION "$CMD_DURATION$cmd_duration"

    __starship_set_job_count

    if contains -- --final-rendering $argv; or test "$TRANSIENT" = "1"
        if test "$TRANSIENT" = "1"
            set -g TRANSIENT 0
            # Clear from cursor to end of screen as `commandline -f repaint` does not do this
            # See https://github.com/fish-shell/fish-shell/issues/8418
            printf \e\[0J
        end
        if type -q starship_transient_prompt_func
            starship_transient_prompt_func --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
        else
            printf "\e[1;32m❯\e[0m "
        end
    else
        # $__starship_cached expands to --cached when async is on (see below)
        # and to nothing when it's off, so this line covers both modes.
        ::STARSHIP:: prompt $__starship_cached --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
    end
end

function fish_right_prompt
    switch "$fish_key_bindings"
        case fish_hybrid_key_bindings fish_vi_key_bindings fish_helix_keybindings
            set STARSHIP_KEYMAP "$fish_bind_mode"
        case '*'
            set STARSHIP_KEYMAP insert
    end

    set STARSHIP_CMD_PIPESTATUS $pipestatus
    set STARSHIP_CMD_STATUS $status
    # Account for changes in variable name between v2.7 and v3.0
    set STARSHIP_DURATION "$CMD_DURATION$cmd_duration"

    # Now it's safe to call job count function (after status capture)
    __starship_set_job_count

    if contains -- --final-rendering $argv; or test "$RIGHT_TRANSIENT" = "1"
        set -g RIGHT_TRANSIENT 0
        if type -q starship_transient_rprompt_func
            starship_transient_rprompt_func --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
        else
            printf ""
        end
    else
        ::STARSHIP:: prompt $__starship_cached --right --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
    end
end

# Disable virtualenv prompt, it breaks starship
set -g VIRTUAL_ENV_DISABLE_PROMPT 1

# Remove default mode prompt
builtin functions -e fish_mode_prompt

set -gx STARSHIP_SHELL "fish"

# Transience related functions
function __starship_reset_transient --on-event fish_postexec
    set -g TRANSIENT 0
    set -g RIGHT_TRANSIENT 0
end

function __starship_transient_execute
    if commandline --is-valid || test -z (commandline | string collect) && not commandline --paging-mode
        set -g TRANSIENT 1
        set -g RIGHT_TRANSIENT 1
        commandline -f repaint
    end
    commandline -f execute
end

function __starship_fish_version_at_least --description 'Check if fish version is at least the given version'
    set -l parts (string split '.' $FISH_VERSION)
    set -l major $parts[1]
    set -l minor 0
    if set -q parts[2]
        set minor $parts[2]
    end

    set req_parts (string split '.' $argv[1])
    set req_major $req_parts[1]
    set req_minor 0
    if set -q req_parts[2]
        set req_minor $req_parts[2]
    end

    if test $major -gt $req_major
        return 0
    else if test $major -eq $req_major -a $minor -ge $req_minor
        return 0
    else
        return 1
    end
end

# --user is the default, but listed anyway to make it explicit.
function enable_transience --description 'enable transient prompt keybindings'
    # fish >= 4.1 has transient prompt support built
    if __starship_fish_version_at_least 4.1
        set -g fish_transient_prompt 1
        return
    end
    bind --user \r __starship_transient_execute
    bind --user -M insert \r __starship_transient_execute
end

# Erase the transient prompt related key bindings.
# --user is the default, but listed anyway to make it explicit.
# Erasing a user binding will revert to the preset.
function disable_transience --description 'remove transient prompt keybindings'
    # fish >= 4.1 has transient prompt support built
    if __starship_fish_version_at_least 4.1
        set -g fish_transient_prompt 0
        return
    end
    bind --user -e \r
    bind --user -M insert -e \r
end

# Async prompt (opt out with STARSHIP_ASYNC=0): the prompt functions above do
# an instant --cached paint, and a single background watcher (__starship_defer)
# refreshes the cache and pokes this session to repaint.
if test "$STARSHIP_ASYNC" != "0"
    # Make every fast paint a --cached render (slow modules served from the
    # cache the background watcher maintains).
    set -g __starship_cached --cached

    # Universal variables are broadcast to every fish process on the machine,
    # not just this session, so the watcher's repaint signal is scoped to a
    # per-session variable name (using this shell's PID, which is stable and
    # unique for its lifetime). $fish_pid replaced the older %self process
    # expansion in fish 4.0; fall back to %self on older fish.
    if __starship_fish_version_at_least 4.0
        set -g __starship_defer_var "__starship_defer_$fish_pid"
    else
        set -g __starship_defer_var "__starship_defer_"%self
    end

    function __starship_defer_repaint --on-variable $__starship_defer_var
        # The watcher poked: repaint, so the prompt functions re-run their
        # --cached paints and pick up the refreshed cache (or advance dynamic
        # modules on a tick). Never repaint over an active transient prompt.
        if test "$TRANSIENT" = "1"; or test "$RIGHT_TRANSIENT" = "1"
            return
        end
        commandline -f repaint
    end

    function __starship_defer_postexec --on-event fish_postexec
        # Snapshot the finished command's context (same capture order as
        # fish_prompt: pipestatus, then status, before either is clobbered),
        # then hand it to a fresh watcher. Firing here -- once per executed
        # command, not in fish_prompt -- keeps repaints from re-firing
        # refreshes.
        set -g STARSHIP_CMD_PIPESTATUS $pipestatus
        set -g STARSHIP_CMD_STATUS $status
        set -g STARSHIP_DURATION "$CMD_DURATION$cmd_duration"
        switch "$fish_key_bindings"
            case fish_hybrid_key_bindings fish_vi_key_bindings fish_helix_key_bindings
                set -g STARSHIP_KEYMAP "$fish_bind_mode"
            case '*'
                set -g STARSHIP_KEYMAP insert
        end
        __starship_set_job_count
        __starship_defer
    end

    # Kill the watcher while a command runs so ticks don't repaint over its
    # output; __starship_defer_postexec starts a fresh one right after.
    function __starship_defer_preexec --on-event fish_preexec
        __starship_defer_kill
    end

    # Cleanup on exit so no watcher or universal variable outlives the
    # session. Only this session's own variable is erased.
    function __starship_defer_cleanup --on-event fish_exit
        __starship_defer_kill
        set -e $__starship_defer_var 2>/dev/null
    end

    # Deliberately do NOT fire the watcher here at source-time: every other
    # launch happens from inside an already-running prompt callback
    # (fish_postexec), after fish has fully returned control from sourcing.
    # The very first prompt therefore renders live (nothing cached yet
    # anyway) and the watcher starts normally once the first command
    # finishes -- this avoids backgrounding a job before fish's own job
    # control/terminal handoff (particularly via the `psub`-based two-phase
    # `starship init fish | source`) has settled.
end

# Set up the session key that will be used to store logs
# We don't use `random [min] [max]` because it is unavailable in older versions of fish shell
set -gx STARSHIP_SESSION_KEY (string sub -s1 -l16 (random)(random)(random)(random)(random)0000000000000000)
