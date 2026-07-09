# ZSH has a quirk where `preexec` is only run if a command is actually run (i.e
# pressing ENTER at an empty command line will not cause preexec to fire). This
# can cause timing issues, as a user who presses "ENTER" without running a command
# will see the time to the start of the last command, which may be very large.

# To fix this, we create STARSHIP_START_TIME upon preexec() firing, and destroy it
# after drawing the prompt. This ensures that the timing for one command is only
# ever drawn once (for the prompt immediately after it is run).

zmodload zsh/parameter  # Needed to access jobstates variable for STARSHIP_JOBS_COUNT

# Defines a function `__starship_get_time` that sets the time since epoch in millis in STARSHIP_CAPTURED_TIME.
if [[ $ZSH_VERSION == ([1-4]*) ]]; then
    # ZSH <= 5; Does not have a built-in variable so we will rely on Starship's inbuilt time function.
    __starship_get_time() {
        STARSHIP_CAPTURED_TIME=$(::STARSHIP:: time)
    }
else
    zmodload zsh/datetime
    zmodload zsh/mathfunc
    __starship_get_time() {
        (( STARSHIP_CAPTURED_TIME = int(rint(EPOCHREALTIME * 1000)) ))
    }
fi

# The two functions below follow the naming convention `prompt_<theme>_<hook>`
# for compatibility with Zsh's prompt system. See
# https://github.com/zsh-users/zsh/blob/2876c25a28b8052d6683027998cc118fc9b50157/Functions/Prompts/promptinit#L155

# Runs before each new command line.
prompt_starship_precmd() {
    # Save the status, because subsequent commands in this function will change $?
    STARSHIP_CMD_STATUS=$? STARSHIP_PIPE_STATUS=(${pipestatus[@]})

    # Calculate duration if a command was executed
    if (( ${+STARSHIP_START_TIME} )); then
        # If an arithmetic expression evaluates to 0, its exit status is 1:
        # "The return status is 0 if the arithmetic value of the expression is non-zero, 1 if it is zero, and 2 if an error occurred."
        # In rare cases, the subtraction below can result in an int 0 result (yes, really),
        # which would then kill the shell if 'set -e' is in effect.
        # We therefore have to assign the result outside the expression (using 'STARSHIP_DURATION=$((...))'),
        # because unlike '(())', '$(())' gets a return status of 0 even if the expression evaluates to int 0
        # (but it still surfaces a potential error, normally status 2, as status 1).
        __starship_get_time && STARSHIP_DURATION=$(( STARSHIP_CAPTURED_TIME - STARSHIP_START_TIME ))
        unset STARSHIP_START_TIME
    # Drop status and duration otherwise
    else
        unset STARSHIP_DURATION STARSHIP_CMD_STATUS STARSHIP_PIPE_STATUS
    fi

    # Use length of jobstates array as number of jobs. Expansion fails inside
    # quotes so we set it here and then use the value later on.
    STARSHIP_JOBS_COUNT="${#jobstates[*]}"
}

# Runs after the user submits the command line, but before it is executed and
# only if there's an actual command to run
prompt_starship_preexec() {
    __starship_get_time && STARSHIP_START_TIME=$STARSHIP_CAPTURED_TIME
}

# Add hook functions
autoload -Uz add-zsh-hook
add-zsh-hook precmd prompt_starship_precmd
add-zsh-hook preexec prompt_starship_preexec

# Set up a function to redraw the prompt if the user switches vi modes
starship_zle-keymap-select() {
    zle reset-prompt
}

## Check for existing keymap-select widget.
if [[ -v widgets[zle-keymap-select] ]]; then
    # zle-keymap-select is a special widget so it'll be "user:fnName" or nothing. Let's get fnName only.
    __starship_preserved_zle_keymap_select=${widgets[zle-keymap-select]#user:}
fi

if [[ -z ${__starship_preserved_zle_keymap_select:-} ]]; then
    zle -N zle-keymap-select starship_zle-keymap-select;
else
    # Define a wrapper fn to call the original widget fn and then Starship's.
    starship_zle-keymap-select-wrapped() {
        $__starship_preserved_zle_keymap_select "$@";
        starship_zle-keymap-select "$@";
    }
    zle -N zle-keymap-select starship_zle-keymap-select-wrapped;
fi

export STARSHIP_SHELL="zsh"

# Set up the session key that will be used to store logs
STARSHIP_SESSION_KEY="$RANDOM$RANDOM$RANDOM$RANDOM$RANDOM"; # Random generates a number b/w 0 - 32767
STARSHIP_SESSION_KEY="${STARSHIP_SESSION_KEY}0000000000000000" # Pad it to 16+ chars.
export STARSHIP_SESSION_KEY=${STARSHIP_SESSION_KEY:0:16}; # Trim to 16-digits if excess.

VIRTUAL_ENV_DISABLE_PROMPT=1

setopt promptsubst

PROMPT='$('::STARSHIP::' prompt --terminal-width="$COLUMNS" --keymap="${KEYMAP:-}" --status="${STARSHIP_CMD_STATUS:-}" --pipestatus="${STARSHIP_PIPE_STATUS[*]:-}" --cmd-duration="${STARSHIP_DURATION:-}" --jobs="$STARSHIP_JOBS_COUNT")'
RPROMPT='$('::STARSHIP::' prompt --right --terminal-width="$COLUMNS" --keymap="${KEYMAP:-}" --status="${STARSHIP_CMD_STATUS:-}" --pipestatus="${STARSHIP_PIPE_STATUS[*]:-}" --cmd-duration="${STARSHIP_DURATION:-}" --jobs="$STARSHIP_JOBS_COUNT")'
PROMPT2="$(::STARSHIP:: prompt --continuation)"

# Async prompt (opt out with STARSHIP_ASYNC=0). Two pieces:
#   1. PROMPT/RPROMPT run `starship prompt --cached`: an instant paint that
#      serves slow modules (git status, ...) from the cache maintained by the
#      background refresh, and computes the fast ones live.
#   2. precmd launches a single background `starship prompt --deferred
#      --watch`, which recomputes everything (both sides), rewrites the cache,
#      and then prints one line per repaint this shell should do: one when the
#      refresh lands, then one per configured `refresh_interval` tick so
#      dynamic modules (e.g. `time`) keep advancing while the prompt sits
#      idle. A single zle -F callback turns each of those lines into
#      reset-prompt, which re-runs the --cached paint from step 1.
if [[ ${STARSHIP_ASYNC:-1} != 0 ]]; then
    PROMPT='$('::STARSHIP::' prompt --cached --terminal-width="$COLUMNS" --keymap="${KEYMAP:-}" --status="${STARSHIP_CMD_STATUS:-}" --pipestatus="${STARSHIP_PIPE_STATUS[*]:-}" --cmd-duration="${STARSHIP_DURATION:-}" --jobs="$STARSHIP_JOBS_COUNT")'
    RPROMPT='$('::STARSHIP::' prompt --cached --right --terminal-width="$COLUMNS" --keymap="${KEYMAP:-}" --status="${STARSHIP_CMD_STATUS:-}" --pipestatus="${STARSHIP_PIPE_STATUS[*]:-}" --cmd-duration="${STARSHIP_DURATION:-}" --jobs="$STARSHIP_JOBS_COUNT")'

    typeset -g _STARSHIP_DEFER_FD=0 _STARSHIP_DEFER_PID=0

    # Stop the current watcher: detach its callback, close its pipe, kill it.
    _starship_defer_cancel() {
        (( _STARSHIP_DEFER_FD )) || return 0
        zle -F "$_STARSHIP_DEFER_FD" 2>/dev/null
        exec {_STARSHIP_DEFER_FD}<&- 2>/dev/null
        (( _STARSHIP_DEFER_PID )) && kill "$_STARSHIP_DEFER_PID" 2>/dev/null
        _STARSHIP_DEFER_FD=0 _STARSHIP_DEFER_PID=0
    }

    # One poke line from the watcher = one repaint (the refresh landed, or a
    # live-update tick elapsed).
    _starship_defer_callback() {
        local fd=$1 _poke
        if ! IFS= read -r -u "$fd" _poke 2>/dev/null; then
            # EOF: the watcher exited (refresh_interval is 0, or it was
            # killed). Detach without kill -- the process is already gone.
            zle -F "$fd" 2>/dev/null
            exec {fd}<&- 2>/dev/null
            if (( fd == _STARSHIP_DEFER_FD )); then
                _STARSHIP_DEFER_FD=0 _STARSHIP_DEFER_PID=0
            fi
            return 0
        fi
        # Avoid disrupting completion menus / incremental search, where a
        # reset-prompt would interfere.
        if [[ -z $WIDGET || ( $WIDGET != *complete* && $WIDGET != *search* ) ]]; then
            zle reset-prompt
        fi
    }

    # precmd: replace any in-flight watcher with a fresh one carrying this
    # command's context.
    _starship_defer_start() {
        _starship_defer_cancel
        exec {_STARSHIP_DEFER_FD}< <(::STARSHIP:: prompt --deferred --watch \
            --terminal-width="$COLUMNS" \
            --keymap="${KEYMAP:-}" \
            --status="${STARSHIP_CMD_STATUS:-}" \
            --pipestatus="${STARSHIP_PIPE_STATUS[*]:-}" \
            --cmd-duration="${STARSHIP_DURATION:-}" \
            --jobs="$STARSHIP_JOBS_COUNT")
        _STARSHIP_DEFER_PID=$!
        zle -F "$_STARSHIP_DEFER_FD" _starship_defer_callback
    }

    add-zsh-hook precmd _starship_defer_start
    add-zsh-hook zshexit _starship_defer_cancel
fi

