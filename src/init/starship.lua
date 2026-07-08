if (clink.version_encoded or 0) < 10020030 then
  error("Starship requires a newer version of Clink; please upgrade to Clink v1.2.30 or later.")
end

-- Async prompt support via clink.promptcoroutine for Cmd (Clink).
--
-- Behavior:
-- - If STARSHIP_ASYNC is unset, default to "1".
-- - Plain `starship prompt` (or --right) runs in CacheRead mode: instant paint
--   using cached values for slow modules.
-- - clink.promptcoroutine launches `starship prompt --async` (Refresh mode) in
--   the background. The coroutine func uses io.popenyield (when inside a
--   coroutine) so it doesn't block prompt filtering or input editing.
-- - When the background render finishes, Clink automatically re-runs prompt
--   filters (repaint) and the coroutine result is returned instead.
-- - Right prompt uses a separate cookie so it has its own coroutine.
--
-- Error handling: all starship invocations are wrapped; failures produce ""
-- rather than crashing the prompt.
-- Older Clink: gracefully falls back (no coroutine => always synchronous
-- direct/fast render, no background refresh/repaint).
-- Transient filters never use coroutines (Clink does not support async during
-- transients).

local use_async = false
local starship_async = os.getenv("STARSHIP_ASYNC")
if starship_async == nil then
  os.setenv("STARSHIP_ASYNC", "1")
  use_async = (clink.promptcoroutine ~= nil)
elseif starship_async ~= "0" and starship_async ~= "" then
  use_async = (clink.promptcoroutine ~= nil)
end

-- Cookie support for multiple coroutines per filter was added in Clink v1.7.0.
local has_cookie = (clink.version_encoded or 0) >= 10070000

local function get_async_prompt(func, cookie)
  if not use_async then
    return nil
  end
  if has_cookie then
    return clink.promptcoroutine(func, cookie)
  end
  -- Pre-1.7.0: only one coroutine slot per promptfilter. Use for left only.
  if cookie == "left" or cookie == nil then
    return clink.promptcoroutine(func)
  end
  return nil
end

local starship_prompt = clink.promptfilter(5)

start_time = os.clock()
end_time = 0
curr_duration = 0
is_line_empty = true

clink.onbeginedit(function()
  end_time = os.clock()
  if not is_line_empty then
    curr_duration = end_time - start_time
  end
end)

clink.onendedit(function(curr_line)
  if starship_precmd_user_func ~= nil then
    starship_precmd_user_func(curr_line)
  end
  start_time = os.clock()
  if string.len(string.gsub(curr_line, '^%s*(.-)%s*$', '%1')) == 0 then
    is_line_empty = true
  else
    is_line_empty = false
  end
end)

-- Safely run starship and return its output (or "" on any error).
-- When async=true, appends --async flag and prefers io.popenyield (yields
-- inside the prompt coroutine for cooperative background execution).
local function run_starship(right, async)
  local ok, output = pcall(function()
    local cmd = "prompt"
    if right then cmd = cmd .. " --right" end
    cmd = cmd
        .. " --status=" .. os.geterrorlevel()
        .. " --cmd-duration=" .. math.floor(curr_duration * 1000)
        .. " --terminal-width=" .. console.getwidth()
        .. " --keymap=" .. (rl.getvariable("keymap") or "")

    if async then
      cmd = cmd .. " --async"
    end

    local full = "::STARSHIP:: " .. cmd
    local popen_fn = (async and io.popenyield) or io.popen
    local f, pclose = popen_fn(full)
    if not f then
      return ""
    end

    local out = f:read("*a") or ""
    if pclose and type(pclose) == "function" then
      pclose() -- pclose closes the handle and may return exit status
    else
      f:close()
    end
    return out
  end)
  return ok and output or ""
end

function starship_prompt:filter(prompt)
  if starship_preprompt_user_func ~= nil then
    starship_preprompt_user_func(prompt)
  end

  local prompt_str = run_starship(false, false)

  -- Launch (or retrieve) async refresh. Returns nil while running.
  -- On completion Clink re-invokes this filter; we then return the full result.
  local async_str = get_async_prompt(function()
    return run_starship(false, true)
  end, "left")

  if async_str ~= nil then
    prompt_str = async_str
  end
  return prompt_str
end

function starship_prompt:rightfilter(prompt)
  local prompt_str = run_starship(true, false)

  local async_str = get_async_prompt(function()
    return run_starship(true, true)
  end, "right")

  if async_str ~= nil then
    prompt_str = async_str
  end
  return prompt_str
end

if starship_transient_prompt_func ~= nil then
  function starship_prompt:transientfilter(prompt)
    return starship_transient_prompt_func(prompt)
  end
end

if starship_transient_rprompt_func ~= nil then
  function starship_prompt:transientrightfilter(prompt)
    return starship_transient_rprompt_func(prompt)
  end
end

local characterset = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
local randomkey = ""
math.randomseed(os.time())
for i = 1, 16 do
  local rand = math.random(#characterset)
  randomkey = randomkey .. string.sub(characterset, rand, rand)
end

os.setenv('STARSHIP_SHELL', 'cmd')
os.setenv('STARSHIP_SESSION_KEY', randomkey)
