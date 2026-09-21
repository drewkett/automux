-- automux-managed-neovim-plugin
-- Installed by `automux install-hooks`; edits may be replaced.
local pane = vim.env.TMUX_PANE
if not pane or not vim.env.TMUX then
  return
end

local directory = vim.trim(vim.fn.system({ "__AUTOMUX_BIN__", "nvim-dir" }))
if vim.v.shell_error ~= 0 or directory == "" then
  return
end
-- The time keeps a reused pid from overwriting a session that has not been
-- restored yet.
local session = directory .. "/" .. vim.fn.getpid() .. "-" .. os.time() .. ".vim"

-- The pane option tells automux which session file belongs to this pane. It
-- lives in the tmux server, so it goes away with the pane.
local function label(value)
  if value then
    vim.fn.system({ "tmux", "set-option", "-p", "-t", pane, "@automux-nvim", value })
  else
    vim.fn.system({ "tmux", "set-option", "-pu", "-t", pane, "@automux-nvim" })
  end
end

local labelled = false
local function write_session()
  local temporary = session .. ".tmp"
  vim.cmd("silent! mksession! " .. vim.fn.fnameescape(temporary))
  os.rename(temporary, session)
  if not labelled then
    label(session)
    labelled = true
  end
end

local pending = false
local function save_session()
  if pending then
    return
  end
  pending = true
  vim.defer_fn(function()
    pending = false
    if vim.v.exiting ~= vim.NIL then
      return
    end
    write_session()
  end, 250)
end

local group = vim.api.nvim_create_augroup("AutomuxSession", { clear = true })
vim.api.nvim_create_autocmd(
  { "VimEnter", "SessionLoadPost", "BufWritePost", "TabNew", "TabClosed", "WinNew", "WinClosed", "DirChanged" },
  { group = group, callback = save_session }
)
vim.api.nvim_create_autocmd("VimLeavePre", {
  group = group,
  callback = function()
    write_session()
    label(nil)
  end,
})
