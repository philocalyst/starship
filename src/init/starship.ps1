#!/usr/bin/env pwsh

# Create a new dynamic module so we don't pollute the global namespace with our functions and
# variables
$null = New-Module starship {
    function Get-Cwd {
        $cwd = Get-Location
        $provider_prefix = "$($cwd.Provider.ModuleName)\$($cwd.Provider.Name)::"
        return @{
            # Resolve the actual/physical path
            # NOTE: ProviderPath is only a physical filesystem path for the "FileSystem" provider
            # E.g. `Dev:\` -> `C:\Users\Joe Bloggs\Dev\`
            Path = $cwd.ProviderPath;
            # Resolve the provider-logical path
            # NOTE: Attempt to trim any "provider prefix" from the path string.
            # E.g. `Microsoft.PowerShell.Core\FileSystem::Dev:\` -> `Dev:\`
            LogicalPath =
                if ($cwd.Path.StartsWith($provider_prefix)) {
                    $cwd.Path.Substring($provider_prefix.Length)
                } else {
                    $cwd.Path
                };
        }
    }

    function Invoke-Native {
        param($Executable, $Arguments)
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo -ArgumentList $Executable -Property @{
            StandardOutputEncoding = [System.Text.Encoding]::UTF8;
            RedirectStandardOutput = $true;
            RedirectStandardError = $true;
            CreateNoWindow = $true;
            UseShellExecute = $false;
        };
        if ($startInfo.ArgumentList.Add) {
            # PowerShell 6+ uses .NET 5+ and supports the ArgumentList property
            # which bypasses the need for manually escaping the argument list into
            # a command string.
            foreach ($arg in $Arguments) {
                $startInfo.ArgumentList.Add($arg);
            }
        }
        else {
            # Build an arguments string which follows the C++ command-line argument quoting rules
            # See: https://docs.microsoft.com/en-us/previous-versions//17w5ykft(v=vs.85)?redirectedfrom=MSDN
            $escaped = $Arguments | ForEach-Object {
                $s = $_ -Replace '(\\+)"','$1$1"'; # Escape backslash chains immediately preceding quote marks.
                $s = $s -Replace '(\\+)$','$1$1';  # Escape backslash chains immediately preceding the end of the string.
                $s = $s -Replace '"','\"';         # Escape quote marks.
                "`"$s`""                           # Quote the argument.
            }
            $startInfo.Arguments = $escaped -Join ' ';
        }
        $process = [System.Diagnostics.Process]::Start($startInfo)

        # Read the output and error streams asynchronously
        # Avoids potential deadlocks when the child process fills one of the buffers
        # https://docs.microsoft.com/en-us/dotnet/api/system.diagnostics.process.standardoutput?view=net-6.0#remarks
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        [System.Threading.Tasks.Task]::WaitAll(@($stdout, $stderr))

        # stderr isn't displayed with this style of invocation
        # Manually write it to console
        if ($stderr.Result.Trim() -ne '') {
            # Write-Error doesn't work here
            $host.ui.WriteErrorLine($stderr.Result)
        }

        $stdout.Result;
    }

    function Enable-TransientPrompt {
        Set-PSReadLineKeyHandler -Key Enter -ScriptBlock {
            $previousOutputEncoding = [Console]::OutputEncoding
            try {
                $parseErrors = $null
                [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$null, [ref]$null, [ref]$parseErrors, [ref]$null)
                if ($parseErrors.Count -eq 0) {
                    $script:TransientPrompt = $true
                    [Console]::OutputEncoding = [Text.Encoding]::UTF8
                    [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
                }
            } finally {
                if ($script:DoesUseLists) {
                    # If PSReadline is set to display suggestion list, this workaround is needed to clear the buffer below
                    # before accepting the current commandline. The max amount of items in the list is 10, so 12 lines
                    # are cleared (10 + 1 more for the prompt + 1 more for current commandline).
                    [Microsoft.PowerShell.PSConsoleReadLine]::Insert("`n" * [math]::Min($Host.UI.RawUI.WindowSize.Height - $Host.UI.RawUI.CursorPosition.Y - 1, 12))
                    [Microsoft.PowerShell.PSConsoleReadLine]::Undo()
                }
                [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
                [Console]::OutputEncoding = $previousOutputEncoding
            }
        }
    }

    function Disable-TransientPrompt {
        Set-PSReadLineKeyHandler -Key Enter -Function AcceptLine
        $script:TransientPrompt = $false
    }

    function global:prompt {
        $origDollarQuestion = $global:?
        $origLastExitCode = $global:LASTEXITCODE

        # Invoke precmd, if specified
        try {
            if (Test-Path function:Invoke-Starship-PreCommand) {
                Invoke-Starship-PreCommand
            }
        } catch {}

        # @ makes sure the result is an array even if single or no values are returned
        $jobs = @(Get-Job | Where-Object { $_.State -eq 'Running' }).Count

        $cwd = Get-Cwd
        $arguments = @(
            "prompt"
            "--path=$($cwd.Path)",
            "--logical-path=$($cwd.LogicalPath)",
            "--terminal-width=$($Host.UI.RawUI.WindowSize.Width)",
            "--jobs=$($jobs)"
        )

        # We start from the premise that the command executed correctly, which covers also the fresh console.
        $lastExitCodeForPrompt = 0
        if ($lastCmd = Get-History -Count 1) {
            # In case we have a False on the Dollar hook, we know there's an error.
            if (-not $origDollarQuestion) {
                # We retrieve the InvocationInfo from the most recent error using $global:error[0]
                $lastCmdletError = try { $global:error[0] |  Where-Object { $_ -ne $null } | Select-Object -ExpandProperty InvocationInfo } catch { $null }
                # We check if the last command executed matches the line that caused the last error, in which case we know
                # it was an internal Powershell command, otherwise, there MUST be an error code.
                $lastExitCodeForPrompt = if ($null -ne $lastCmdletError -and $lastCmd.CommandLine -eq $lastCmdletError.Line) { 1 } else { $origLastExitCode }
            }
            $duration = [math]::Round(($lastCmd.EndExecutionTime - $lastCmd.StartExecutionTime).TotalMilliseconds)

            $arguments += "--cmd-duration=$($duration)"
        }

        $arguments += "--status=$($lastExitCodeForPrompt)"

        if ([Microsoft.PowerShell.PSConsoleReadLine]::InViCommandMode()) {
            $arguments += "--keymap=vi"
        }

        # Invoke Starship
        $promptText = if ($script:TransientPrompt) {
            $script:TransientPrompt = $false
            if (Test-Path function:Invoke-Starship-TransientFunction) {
                Invoke-Starship-TransientFunction
            } else {
                "$([char]0x1B)[1;32m❯$([char]0x1B)[0m "
            }
        } else {
            $p = Invoke-Native -Executable ::STARSHIP:: -Arguments $arguments
            __starship_fire_async $arguments
            $p
        }

        # Set the number of extra lines in the prompt for PSReadLine prompt redraw.
        Set-PSReadLineOption -ExtraPromptLineCount ($promptText.Split("`n").Length - 1)

        # Return the prompt
        $promptText

        # Propagate the original $LASTEXITCODE from before the prompt function was invoked.
        $global:LASTEXITCODE = $origLastExitCode

        # Propagate the original $? automatic variable value from before the prompt function was invoked.
        #
        # $? is a read-only or constant variable so we can't directly override it.
        # In order to propagate up its original boolean value we will take an action
        # which will produce the desired value.
        #
        # This has to be the very last thing that happens in the prompt function
        # since every PowerShell command sets the $? variable.
        if ($global:? -ne $origDollarQuestion) {
            if ($origDollarQuestion) {
                 # Simple command which will execute successfully and set $? = True without any other side affects.
                1+1
            } else {
                # Write-Error will set $? to False.
                # ErrorAction Ignore will prevent the error from being added to the $Error collection.
                Write-Error '' -ErrorAction 'Ignore'
            }
        }

    }

    # Disable virtualenv prompt, it breaks starship
    $ENV:VIRTUAL_ENV_DISABLE_PROMPT=1

    $script:TransientPrompt = $false
    $script:DoesUseLists = (Get-PSReadLineOption).PredictionViewStyle -eq 'ListView'

    if ($PSVersionTable.PSVersion.Major -gt 5) {
        $ENV:STARSHIP_SHELL = "pwsh"
    } else {
        $ENV:STARSHIP_SHELL = "powershell"
    }

    # Set up the session key that will be used to store logs
    $ENV:STARSHIP_SESSION_KEY = -join ((48..57) + (65..90) + (97..122) | Get-Random -Count 16 | ForEach-Object { [char]$_ })

    # Async prompt support (in-place repaint via PSReadLine::InvokePrompt).
    # Plain prompt calls become CacheRead when STARSHIP_ASYNC=1.
    # We fire background --async jobs (Refresh) to populate cache, then repaint
    # the prompt in place (without waiting for the next line) once the job
    # finishes, mirroring the async repaint bash/zsh/fish implement natively.
    if (-not (Test-Path Env:STARSHIP_ASYNC) -or [string]::IsNullOrEmpty($env:STARSHIP_ASYNC)) {
        $env:STARSHIP_ASYNC = "1"
    }

    # In-place repaint requires PSReadLine's InvokePrompt (PSReadLine 2.1+).
    # If it isn't available we still fire the async job (it's harmless and
    # keeps the cache warm), we just can't repaint until the next prompt draw.
    #
    # IMPORTANT, confirmed by direct testing: on this platform (verified on
    # pwsh 7.7-preview/macOS; PSReadLine/PSReadLine#1092 reports the same
    # class of issue), PowerShell.OnIdle stops firing for the rest of the
    # session as soon as the background job below launches the real
    # `starship prompt --async` child process -- this reproduces identically
    # whether the process is launched via Start-ThreadJob, Start-Job, or
    # System.Diagnostics.Process directly from the main thread, so it is not
    # fixable by changing how the job is started. In practice this means the
    # OnIdle handler registered in __starship_fire_async below is very likely
    # to never fire on non-Windows hosts once real work is queued, and the
    # cache-warm value is only picked up on the *next* natural prompt draw
    # instead of being repainted in place. That degraded behavior is still
    # correct (never stale, never wrong -- just not instant), so the handler
    # is left in place as a best-effort win for hosts where it does work,
    # rather than special-cased off by platform.

    # The background refresh must be a Start-ThreadJob, not a Start-Job: a
    # Start-Job's .State is a remoting-serialized snapshot that, verified by
    # direct testing, never updates as observed from inside a
    # Register-EngineEvent/PowerShell.OnIdle action in this process -- the
    # OnIdle handler would poll a Start-Job forever and never see it leave
    # "Running", so the repaint (and the cache-populating job itself) would
    # never be reaped. Start-ThreadJob runs in-process with no serialization
    # layer, so its .State is visible immediately from any scope. ThreadJob
    # ships with PowerShell 7+ but isn't guaranteed on older installs; if it's
    # missing, skip the OnIdle/repaint machinery entirely (same fallback
    # philosophy as $__starship_can_repaint above) rather than register a
    # handler that can never fire.
    $script:__starship_has_threadjob = $null -ne (Get-Command Start-ThreadJob -ErrorAction SilentlyContinue)
    $script:__starship_can_repaint = $script:__starship_can_repaint -and $script:__starship_has_threadjob

    $script:__starship_async_job = $null

    function __starship_cleanup_async {
        # Removes the tracked job regardless of state (Running/Completed/
        # Failed/Stopped), so finished jobs never pile up in `Get-Job`.
        #
        # Deliberately does NOT use Register-ObjectEvent on the job itself:
        # in testing, a live event subscription bound to a Start-Job object
        # that is still pending when the pwsh runspace closes (session exit)
        # reliably crashes the whole process (PSObjectDisposedException /
        # FailFast during RunspaceClosingNotification, since the event
        # manager's teardown races the job's own disposal). Polling via
        # PowerShell.OnIdle (see below) never binds an event to the job, so
        # it doesn't hit that teardown race.
        Unregister-Event -SourceIdentifier PowerShell.OnIdle -ErrorAction SilentlyContinue
        Remove-Job -Name PowerShell.OnIdle -Force -ErrorAction SilentlyContinue
        if ($script:__starship_async_job) {
            if ($script:__starship_async_job.State -eq 'Running') {
                Stop-Job $script:__starship_async_job -ErrorAction SilentlyContinue
            }
            Receive-Job $script:__starship_async_job -ErrorAction SilentlyContinue | Out-Null
            Remove-Job $script:__starship_async_job -Force -ErrorAction SilentlyContinue
            $script:__starship_async_job = $null
        }
    }

    # Register-EngineEvent's -Action scriptblock runs disconnected from
    # module scope: neither $script:-scoped variables nor $Event.MessageData
    # reliably resolve inside an inline anonymous action block in testing
    # (both came back empty/null when read directly inside -Action). A
    # scriptblock stored in a *named function* and invoked BY NAME from
    # -Action does work, because the function itself carries the closure
    # over module scope from where it was defined -- so the action below is
    # just a thin `& __starship_onidle_check`, and all the real logic (and
    # $script: state access) lives in the function, not the action block.
    function __starship_onidle_check {
        if (-not $script:__starship_async_job) { return }
        # 'NotStarted' as well as 'Running' both mean "not done yet" -- a
        # freshly created job can be observed in 'NotStarted' for one or two
        # ticks before its worker thread actually begins, and treating that
        # as "done" would reap/repaint on a job that never actually ran
        # (confirmed by direct testing: a same-tick re-fire can otherwise be
        # seen in 'NotStarted' and mishandled as complete).
        if ($script:__starship_async_job.State -in @('NotStarted', 'Running')) {
            # A single registration does keep recurring on its own on later
            # idle ticks, so this re-arm is belt-and-suspenders (harmless:
            # the null-guard above makes any duplicate firing a no-op once
            # $script:__starship_async_job is cleared below).
            Register-EngineEvent -SourceIdentifier PowerShell.OnIdle -SupportEvent -Action { __starship_onidle_check } | Out-Null
            return
        }
        Receive-Job $script:__starship_async_job -ErrorAction SilentlyContinue | Out-Null
        Remove-Job $script:__starship_async_job -Force -ErrorAction SilentlyContinue
        $script:__starship_async_job = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    }

    function __starship_fire_async {
        param($arguments)
        if ($env:STARSHIP_ASYNC -eq "0") { return }

        # Cancel/clean up any still-pending previous job (and its OnIdle
        # polling registration) so rapid Enter-Enter never leaves orphaned
        # Start-Job processes or event subscriptions behind.
        __starship_cleanup_async

        $asyncArgs = $arguments + @("--async")
        # NOTE: the scriptblock parameter must not be named $args -- that
        # collides with PowerShell's automatic "extra arguments" variable of
        # the same name, which silently wins inside the job's scriptblock
        # scope and left the splat empty (the async invocation was calling
        # starship with zero arguments, printing usage help instead of a
        # prompt, and never actually populating the cache).
        #
        # Use Start-ThreadJob when available (see $__starship_has_threadjob
        # above for why); Start-Job still populates the cache correctly when
        # ThreadJob is missing, we just can't observe its completion to
        # repaint in place, so $__starship_can_repaint is already false in
        # that case and no OnIdle handler gets registered against it.
        $startJobCmd = if ($script:__starship_has_threadjob) { 'Start-ThreadJob' } else { 'Start-Job' }
        $script:__starship_async_job = & $startJobCmd -ScriptBlock {
            param($exe, $cliArgs)
            & $exe @cliArgs | Out-Null
        } -ArgumentList ::STARSHIP::, $asyncArgs

        if ($script:__starship_can_repaint) {
            # Repaint by re-invoking the real prompt function via the same
            # InvokePrompt mechanism Enable-TransientPrompt already uses
            # above, which will pick up the now-warm cache.
            Register-EngineEvent -SourceIdentifier PowerShell.OnIdle -SupportEvent -Action { __starship_onidle_check } | Out-Null
        }
    }

    # Clean up the background job and event subscription when the module is
    # removed (e.g. on session exit), so nothing is left running/registered.
    $ExecutionContext.SessionState.Module.OnRemove = {
        __starship_cleanup_async
    }

    # Invoke Starship and set continuation prompt
    Set-PSReadLineOption -ContinuationPrompt (
        Invoke-Native -Executable ::STARSHIP:: -Arguments @(
            "prompt",
            "--continuation"
        )
    )

    try {
        # Combine user defined ViModeChangeHandler if it exists
        if((Get-PSReadLineOption).ViModeChangeHandler){
            # &{...} to limit the scope of the GetNewClosure
            & {
                $originalHandler = (Get-PSReadLineOption).ViModeChangeHandler
                Set-PSReadLineOption -ViModeChangeHandler {
                    [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
                    & $originalHandler @args
                }.GetNewClosure()
            }
        } else {
            Set-PSReadLineOption -ViModeIndicator script -ViModeChangeHandler {
                [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
            }
        }
    } catch {}

    Export-ModuleMember -Function @(
        "Enable-TransientPrompt"
        "Disable-TransientPrompt"
    )
}
