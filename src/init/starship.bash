# We use PROMPT_COMMAND and the DEBUG trap to generate timing information. We try
# to avoid clobbering what we can, and try to give the user ways around our
# clobbers, if it's unavoidable. For example, PROMPT_COMMAND is appended to,
# and the DEBUG trap is layered with other traps, if it exists.

# A bash quirk is that the DEBUG trap is fired every time a command runs, even
# if it's later on in the pipeline. If uncorrected, this could cause bad timing
# data for commands like `slow | slow | fast`, since the timer starts at the start
# of the "fast" command.

# To solve this, we set a flag `STARSHIP_PREEXEC_READY` when the prompt is
# drawn, and only start the timer if this flag is present. That way, timing is
# for the entire command, and not just a portion of it.

# A way to set '$?', since bash does not allow assigning to '$?' directly
function _starship_set_return() { return "${1:-0}"; }

# Will be run before *every* command (even ones in pipes!)
starship_preexec() {
    # Save previous command's last argument, otherwise it will be set to "starship_preexec"
    local PREV_LAST_ARG=$1

    # Avoid restarting the timer for commands in the same pipeline
    if [ "${STARSHIP_PREEXEC_READY:-}" = "true" ]; then
        STARSHIP_PREEXEC_READY=false
        STARSHIP_START_TIME=$(::STARSHIP:: time)
    fi

    : "$PREV_LAST_ARG"
}

# Will be run before the prompt is drawn
starship_precmd() {
    # Save the status, because commands in this pipeline will change $?
    STARSHIP_CMD_STATUS=$? STARSHIP_PIPE_STATUS=("${PIPESTATUS[@]}")
    if [[ ${BLE_ATTACHED-} && ${#BLE_PIPESTATUS[@]} -gt 0 ]]; then
        STARSHIP_PIPE_STATUS=("${BLE_PIPESTATUS[@]}")
    fi
    if [[ -n "${BP_PIPESTATUS-}" ]] && [[ "${#BP_PIPESTATUS[@]}" -gt 0 ]]; then
        STARSHIP_PIPE_STATUS=("${BP_PIPESTATUS[@]}")
    fi

    # Due to a bug in certain Bash versions, any external process launched
    # inside $PROMPT_COMMAND will be reported by `jobs` as a background job:
    #
    #   [1]  42135 Done                    /bin/echo
    #
    # This is a workaround - we run `jobs` once to clear out any completed jobs
    # first, and then we run it again and count the number of jobs.
    #
    # More context: https://github.com/starship/starship/issues/5159
    # Original bug: https://lists.gnu.org/archive/html/bug-bash/2022-07/msg00117.html
    jobs &>/dev/null

    local job NUM_JOBS=0 IFS=$' \t\n'
    # Evaluate the number of jobs before running the preserved prompt command, so that tools
    # like z/autojump, which background certain jobs, do not cause spurious background jobs
    # to be displayed by starship. Also avoids forking to run `wc`, slightly improving perf.
    for job in $(jobs -p); do [[ $job ]] && ((NUM_JOBS++)); done

    # Run the bash precmd function, if it's set. If not set, evaluates to no-op
    "${starship_precmd_user_func-:}"

    # Set $? to the preserved value before running additional parts of the prompt
    # command pipeline, which may rely on it.
    _starship_set_return "$STARSHIP_CMD_STATUS"

    if [[ -n "${STARSHIP_PROMPT_COMMAND-}" ]]; then
        eval "$STARSHIP_PROMPT_COMMAND"
    fi

    local -a ARGS=(--terminal-width="${COLUMNS}" --status="${STARSHIP_CMD_STATUS}" --pipestatus="${STARSHIP_PIPE_STATUS[*]}" --jobs="${NUM_JOBS}" --shlvl="${SHLVL}")
    # Prepare the timer data, if needed.
    if [[ -n "${STARSHIP_START_TIME-}" ]]; then
        STARSHIP_END_TIME=$(::STARSHIP:: time)
        STARSHIP_DURATION=$((STARSHIP_END_TIME - STARSHIP_START_TIME))
        ARGS+=( --cmd-duration="${STARSHIP_DURATION}")
        STARSHIP_START_TIME=""
    fi

    # Async support: always compute the "instant" (CacheRead under STARSHIP_ASYNC=1)
    # prompt first for responsiveness. If async is on, set PS1 to a getter that
    # adopts the full result once it lands, and launch the --async refresh in
    # the background (atomic write) to populate the cache for it.
    #
    # Note for plain Bash: there is no portable way to force readline to
    # re-expand PS1 mid-line, so the refreshed prompt is only picked up on the
    # *next* prompt draw (i.e. after the next command), not while the current
    # line is being edited. We still signal WINCH because ble.sh (if attached)
    # has its own redraw path that can act on it; in plain Bash it is a no-op
    # beyond bookkeeping ($COLUMNS/$LINES via `checkwinsize`).
    local prompt
    prompt="$(::STARSHIP:: prompt "${ARGS[@]}")"
    if [[ ${STARSHIP_ASYNC:-0} != 0 ]]; then
        _starship_async_cancel
        _STARSHIP_INSTANT_PROMPT=${prompt}
        _STARSHIP_ASYNC_PROMPT=
        PS1='$(_starship_get_prompt)'
        local tmp
        tmp=$(mktemp -t "starship-async.$$.XXXXXX" 2>/dev/null || echo "/tmp/starship-async.$$.$RANDOM")
        _STARSHIP_ASYNC_TMP=${tmp}
        (
            ::STARSHIP:: prompt --async "${ARGS[@]}" >"${tmp}.tmp" && mv -f "${tmp}.tmp" "${tmp}"
            kill -WINCH $$ 2>/dev/null || true
        ) &
        _STARSHIP_ASYNC_PID=$!
        disown "${_STARSHIP_ASYNC_PID}" 2>/dev/null || true
    else
        PS1=${prompt}
    fi
    if [[ ${BLE_ATTACHED-} ]]; then
        local nlns=${prompt//[!$'\n']}
        bleopt prompt_rps1="$nlns$(::STARSHIP:: prompt --right "${ARGS[@]}")"
    fi
    STARSHIP_PREEXEC_READY=true  # Signal that we can safely restart the timer
}

# If the user appears to be using https://github.com/akinomyoga/ble.sh,
# then hook our functions into their framework.
if [[ ${BLE_VERSION-} && _ble_version -ge 400 ]]; then
    blehook PREEXEC!='starship_preexec "$_"'
    blehook PRECMD!='starship_precmd'
# If the user appears to be using https://github.com/rcaloras/bash-preexec,
# then hook our functions into their framework.
elif [[ -n "${bash_preexec_imported:-}" || -n "${__bp_imported:-}" || -n "${preexec_functions-}" || -n "${precmd_functions-}" ]]; then
    # bash-preexec needs a single function--wrap the args into a closure and pass
    starship_preexec_all(){ starship_preexec "$_"; }
    preexec_functions+=(starship_preexec_all)
    precmd_functions+=(starship_precmd)
else
    if [[ -n "${BASH_VERSION-}" ]] && [[ "${BASH_VERSINFO[0]}" -gt 4 || ( "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 ) ]]; then
        starship_preexec_ps0() {
            ::STARSHIP:: time
        }
        # In order to set STARSHIP_START_TIME use an arithmetic expansion that evaluates to 0
        # To avoid printing anything, use the return value in an ${var:offset:length} substring expansion
        # with offset and length evaluating to 0.
        PS0='${STARSHIP_START_TIME:$((STARSHIP_START_TIME="$(starship_preexec_ps0)",STARSHIP_PREEXEC_READY=0,0)):0}'"${PS0-}"
    else
        # We want to avoid destroying an existing DEBUG hook. If we detect one, create
        # a new function that runs both the existing function AND our function, then
        # re-trap DEBUG to use this new function. This prevents a trap clobber.
        eval "STARSHIP_DEBUG_TRAP=($(trap -p DEBUG))"
        STARSHIP_DEBUG_TRAP=("${STARSHIP_DEBUG_TRAP[2]}")
        if [[ -z "$STARSHIP_DEBUG_TRAP" ]]; then
            trap 'starship_preexec "$_"' DEBUG
        elif [[ "$STARSHIP_DEBUG_TRAP" != 'starship_preexec "$_"' && "$STARSHIP_DEBUG_TRAP" != 'starship_preexec_all "$_"' ]]; then
            starship_preexec_all() {
                local PREV_LAST_ARG=$1 ; eval -- "$STARSHIP_DEBUG_TRAP"; starship_preexec; : "$PREV_LAST_ARG";
            }
            trap 'starship_preexec_all "$_"' DEBUG
        fi
    fi

    # Finally, prepare the precmd function and set up the start time. We will avoid to
    # add multiple instances of the starship function and keep other user functions if any.
    if [[ -z "${PROMPT_COMMAND-}" ]]; then
        PROMPT_COMMAND="starship_precmd"
    elif [[ "$PROMPT_COMMAND" != *"starship_precmd"* ]]; then
        # Appending to PROMPT_COMMAND breaks exit status ($?) checking.
        # Prepending to PROMPT_COMMAND breaks "command duration" module.
        # So, we are preserving the existing PROMPT_COMMAND
        # which will be executed later in the starship_precmd function
        STARSHIP_PROMPT_COMMAND="$PROMPT_COMMAND"
        PROMPT_COMMAND="starship_precmd"
    fi
fi

# Ensure that $COLUMNS gets set
shopt -s checkwinsize

# Set up the start time and STARSHIP_SHELL, which controls shell-specific sequences
STARSHIP_START_TIME=$(::STARSHIP:: time)
export STARSHIP_SHELL="bash"

# Enable async prompt support by default. A plain `starship prompt` becomes the
# instant (CacheRead) paint; `starship prompt --async` is the background refresh.
: ${STARSHIP_ASYNC:=1}
export STARSHIP_ASYNC

# Set up the session key that will be used to store logs
STARSHIP_SESSION_KEY="$RANDOM$RANDOM$RANDOM$RANDOM$RANDOM"; # Random generates a number b/w 0 - 32767
STARSHIP_SESSION_KEY="${STARSHIP_SESSION_KEY}0000000000000000" # Pad it to 16+ chars.
export STARSHIP_SESSION_KEY=${STARSHIP_SESSION_KEY:0:16}; # Trim to 16-digits if excess.

# Set the continuation prompt
PS2="$(::STARSHIP:: prompt --continuation)"

# Async repainting support for Bash using STARSHIP_ASYNC and --async.
# - STARSHIP_ASYNC=1 (default) makes plain `prompt` a fast CacheRead paint.
# - We launch `prompt --async` (Refresh) in the background to populate the
#   cache and hand back the full prompt text.
# Full integration: starship_precmd (used by PROMPT_COMMAND, the ble.sh
# PRECMD hook, and bash-preexec precmd_functions) handles launch/cancel.
# Correctness:
# - Repaint: the background job writes atomically then sends SIGWINCH. Plain
#   Bash has no way to force readline to re-expand PS1 mid-line, so this only
#   makes the refreshed prompt visible starting with the *next* prompt draw
#   (see _starship_get_prompt); ble.sh, if attached, can act on WINCH sooner.
# - Live updates (root `refresh_interval`): not wired for Bash for the same
#   reason -- readline won't re-expand PS1 mid-line, so a periodic timer can't
#   advance a live module (e.g. `time`) while the user sits at the prompt
#   without a keypress. The interval is therefore a no-op here.
# - Job counting: launched after NUM_JOBS calc; disown prevents it from
#   appearing in `jobs` for subsequent counts (combined with the jobs
#   &>/dev/null workaround already present).
# - Cancellation: _starship_async_cancel kills the whole process group we
#   launched (the background job forks the actual `starship` child, so
#   killing just the job's own pid would leak it) and removes any tmp file
#   before the next launch (handles rapid prompts, ble hooks, etc.).
if [[ ${STARSHIP_ASYNC:-0} != 0 ]]; then
    _STARSHIP_ASYNC_PID=0
    _STARSHIP_ASYNC_TMP=
    _STARSHIP_INSTANT_PROMPT=
    _STARSHIP_ASYNC_PROMPT=

    _starship_async_cancel() {
        if [[ -n ${_STARSHIP_ASYNC_PID} ]] && [[ ${_STARSHIP_ASYNC_PID} != 0 ]] && kill -0 "${_STARSHIP_ASYNC_PID}" 2>/dev/null; then
            # The backgrounded job is its own process group leader (Bash job
            # control), so -PID kills it and the `starship --async` child it
            # forked. Fall back to killing just the job if that fails (e.g.
            # job control unavailable), so we never leak the wrapper either.
            kill -- "-${_STARSHIP_ASYNC_PID}" 2>/dev/null || true
            kill "${_STARSHIP_ASYNC_PID}" 2>/dev/null || true
        fi
        _STARSHIP_ASYNC_PID=0
        if [[ -n ${_STARSHIP_ASYNC_TMP} ]]; then
            rm -f "${_STARSHIP_ASYNC_TMP}" "${_STARSHIP_ASYNC_TMP}.tmp" 2>/dev/null || true
            _STARSHIP_ASYNC_TMP=
        fi
    }

    # Make sure a still-running refresh and its tmp file don't outlive the
    # shell. Layer onto any existing EXIT trap instead of clobbering it.
    eval "STARSHIP_EXIT_TRAP=($(trap -p EXIT))"
    STARSHIP_EXIT_TRAP=("${STARSHIP_EXIT_TRAP[2]}")
    if [[ -z "$STARSHIP_EXIT_TRAP" ]]; then
        trap '_starship_async_cancel' EXIT
    elif [[ "$STARSHIP_EXIT_TRAP" != *"_starship_async_cancel"* ]]; then
        trap "${STARSHIP_EXIT_TRAP}"$'\n''_starship_async_cancel' EXIT
    fi

    _starship_get_prompt() {
        # If a refresh completed (pid gone and file present from mv), adopt it.
        if [[ -n ${_STARSHIP_ASYNC_TMP} && -f ${_STARSHIP_ASYNC_TMP} ]] && ! kill -0 "${_STARSHIP_ASYNC_PID}" 2>/dev/null; then
            _STARSHIP_ASYNC_PROMPT=$(cat -- "${_STARSHIP_ASYNC_TMP}")
            PS1=${_STARSHIP_ASYNC_PROMPT}
            rm -f "${_STARSHIP_ASYNC_TMP}" 2>/dev/null || true
            _STARSHIP_ASYNC_TMP=
            _STARSHIP_ASYNC_PID=0
            if [[ ${BLE_ATTACHED-} ]]; then
                # ble.sh: update its prompt structures and force repaint for async result
                bleopt prompt_ps1="${_STARSHIP_ASYNC_PROMPT}" 2>/dev/null || true
                ble/prompt/update 2>/dev/null || true
            fi
            printf %s "${_STARSHIP_ASYNC_PROMPT}"
            return
        fi
        if [[ -n ${_STARSHIP_ASYNC_PROMPT} ]]; then
            printf %s "${_STARSHIP_ASYNC_PROMPT}"
        else
            printf %s "${_STARSHIP_INSTANT_PROMPT}"
        fi
    }
fi

