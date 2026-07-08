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

function __starship_fire_async --argument-names cmd_status cmd_pipestatus keymap duration jobs
    # Fire a background `starship prompt --async` (Refresh) using the provided
    # context. Its stdout is discarded; it exists to (re)populate the cache.
    # We also fire one for --right so that fish_right_prompt benefits.
    # After both complete we twiddle a universal var; the --on-variable handler
    # then does `commandline -f repaint` so the next prompt draw (via CacheRead)
    # will see fresh cached values.
    #
    # The ready variable is scoped by this session's PID ($fish_pid, captured
    # in $__starship_async_ready_var below). Universal variables are shared by
    # every fish process on the machine, so an unscoped name would make one
    # session's completed refresh trigger repaints (and eventually erasure
    # events) in every other unrelated fish session/tab.
    # Kill any previous bg refresh (for rapid successive prompts).
    __starship_kill_async
    env \
        COLUMNS="$COLUMNS" \
        STARSHIP_CMD_STATUS="$cmd_status" \
        STARSHIP_CMD_PIPESTATUS="$cmd_pipestatus" \
        STARSHIP_KEYMAP="$keymap" \
        STARSHIP_DURATION="$duration" \
        STARSHIP_JOBS="$jobs" \
        STARSHIP_ASYNC_READY_VAR="$__starship_async_ready_var" \
        fish -c '
            ::STARSHIP:: prompt --async \
                --terminal-width="$COLUMNS" \
                --status="$STARSHIP_CMD_STATUS" \
                --pipestatus="$STARSHIP_CMD_PIPESTATUS" \
                --keymap="$STARSHIP_KEYMAP" \
                --cmd-duration="$STARSHIP_DURATION" \
                --jobs="$STARSHIP_JOBS" >/dev/null 2>&1
            ::STARSHIP:: prompt --right --async \
                --terminal-width="$COLUMNS" \
                --status="$STARSHIP_CMD_STATUS" \
                --pipestatus="$STARSHIP_CMD_PIPESTATUS" \
                --keymap="$STARSHIP_KEYMAP" \
                --cmd-duration="$STARSHIP_DURATION" \
                --jobs="$STARSHIP_JOBS" >/dev/null 2>&1
            set -U $STARSHIP_ASYNC_READY_VAR (random)(random)
        ' &
    set -g __starship_async_pid $last_pid
    disown $last_pid 2>/dev/null
end

function __starship_kill_async
    if set -q __starship_async_pid
        kill $__starship_async_pid 2>/dev/null
        set -e __starship_async_pid
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
        ::STARSHIP:: prompt --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
        if test "$STARSHIP_ASYNC" != "0"
            if test "$__starship_async_skip_fire" = "1"
                set -g __starship_async_skip_fire 0
            else
                __starship_fire_async $STARSHIP_CMD_STATUS "$STARSHIP_CMD_PIPESTATUS" $STARSHIP_KEYMAP "$STARSHIP_DURATION" "$STARSHIP_JOBS"
            end
        end
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
        ::STARSHIP:: prompt --right --terminal-width="$COLUMNS" --status=$STARSHIP_CMD_STATUS --pipestatus="$STARSHIP_CMD_PIPESTATUS" --keymap=$STARSHIP_KEYMAP --cmd-duration=$STARSHIP_DURATION --jobs=$STARSHIP_JOBS
        # Participate in async state (skip_fire) so fish_right_prompt is fully
        # integrated with the async sketch (clear on repaint render). Firing
        # (incl. right async) stays in left for single launch per cycle.
        if test "$STARSHIP_ASYNC" != "0"
            if test "$__starship_async_skip_fire" = "1"
                set -g __starship_async_skip_fire 0
            end
        end
    end
end

# Disable virtualenv prompt, it breaks starship
set -g VIRTUAL_ENV_DISABLE_PROMPT 1

# Remove default mode prompt
builtin functions -e fish_mode_prompt

set -gx STARSHIP_SHELL "fish"

# Default to async prompt support (opt-out: set STARSHIP_ASYNC=0).
set -q STARSHIP_ASYNC; or set -gx STARSHIP_ASYNC 1
set -gx STARSHIP_ASYNC $STARSHIP_ASYNC

# Transience related functions
function __starship_reset_transient --on-event fish_postexec
    set -g TRANSIENT 0
    set -g RIGHT_TRANSIENT 0
    # Reset async skip so the following normal prompt (left or right) will fire
    # the bg refresh (covering fish_right_prompt async too). Prevents transients
    # from leaving stale skip_fire that would suppress async after the cmd.
    if test "$STARSHIP_ASYNC" != "0"
        set -g __starship_async_skip_fire 0
    end
end

function __starship_transient_execute
    if commandline --is-valid || test -z (commandline | string collect) && not commandline --paging-mode
        set -g TRANSIENT 1
        set -g RIGHT_TRANSIENT 1
        # Protect async state: clear skip and kill pending refresh so a transient
        # repaint/execute does not race or leave skip=1 that would disable async
        # for the command that follows.
        if test "$STARSHIP_ASYNC" != "0"
            set -g __starship_async_skip_fire 0
            if functions -q __starship_kill_async
                __starship_kill_async
            end
        end
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

if test "$STARSHIP_ASYNC" != "0"
    set -g __starship_async_skip_fire 0

    # Universal variables are broadcast to every fish process on the machine,
    # not just this session, so the "async refresh finished" signal is scoped
    # to a per-session variable name (using this shell's PID, which is stable
    # and unique for its lifetime). This keeps unrelated fish sessions/tabs
    # from repainting (or firing the erasure event on exit, see cleanup below)
    # when this session's background refresh completes.
    # $fish_pid replaced the older %self process expansion in fish 4.0; fall
    # back to %self on older fish so this still works pre-4.0.
    if __starship_fish_version_at_least 4.0
        set -g __starship_async_ready_var "__starship_async_ready_$fish_pid"
    else
        set -g __starship_async_ready_var "__starship_async_ready_"%self
    end

    function __starship_async_repaint --on-variable $__starship_async_ready_var
        # A background refresh finished; repaint so fish_prompt / fish_right_prompt
        # re-run and pick up updated values via (fast) plain prompt CacheRead.
        # Transient bypass: do not repaint if a transient is active.
        if test "$TRANSIENT" = "1"; or test "$RIGHT_TRANSIENT" = "1"
            return
        end
        set -g __starship_async_skip_fire 1
        commandline -f repaint
    end

    # Cleanup on exit to avoid leaking uvars or pending jobs. Erase this
    # session's own ready var only -- never touch other sessions' entries.
    function __starship_async_cleanup --on-event fish_exit
        if functions -q __starship_kill_async
            __starship_kill_async
        end
        set -e $__starship_async_ready_var 2>/dev/null
    end
end

# Set up the session key that will be used to store logs
# We don't use `random [min] [max]` because it is unavailable in older versions of fish shell
set -gx STARSHIP_SESSION_KEY (string sub -s1 -l16 (random)(random)(random)(random)(random)0000000000000000)
