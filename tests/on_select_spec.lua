---@diagnostic disable: undefined-field, missing-fields
-- Tests for the `on_select` user hook introduced to let callers override what
-- fff does when an item is picked (e.g. to avoid :edit targeting a window
-- with 'winfixbuf' set, to send selections to a custom sink, etc.).

local fff_rust = require('fff.rust')
local picker_ui = require('fff.picker_ui')
local file_picker = require('fff.file_picker')
local conf = require('fff.conf')

local function norm(p)
  local rp = vim.uv.fs_realpath(p) or vim.fn.fnamemodify(vim.fn.resolve(p), ':p')
  local n = vim.fs.normalize(rp)
  n = n:gsub('/$', '')
  if vim.fn.has('win32') == 1 then n = n:lower() end
  return n
end

local function wait_for_reindex(expected_dir, timeout_ms)
  local expected = norm(expected_dir)
  local deadline = vim.uv.hrtime() + timeout_ms * 1e6
  while vim.uv.hrtime() < deadline do
    local ok, health = pcall(fff_rust.health_check, expected)
    if ok and health and health.file_picker and health.file_picker.base_path then
      if norm(health.file_picker.base_path) == expected then return true end
    end
    vim.wait(20, function() return false end)
  end
  return false
end

local function wait_for_scan(expected_dir, timeout_ms)
  assert.is_true(wait_for_reindex(expected_dir, timeout_ms), 'reindex did not complete')
  fff_rust.wait_for_initial_scan(timeout_ms)
end

--- Populate picker_ui.state as open_ui_with_state would for a given item list
--- and selected filename, then drive select(action). Mirrors the approach in
--- picker_dir_resolution_spec.lua.
local function drive_select(items, filename, action, on_select)
  picker_ui.state.active = true
  picker_ui.state.filtered_items = items
  picker_ui.state.cursor = (function()
    for i, item in ipairs(items) do
      if item.name == filename then return i end
    end
    return 1
  end)()
  picker_ui.state.query = ''
  picker_ui.state.mode = nil
  picker_ui.state.location = nil
  picker_ui.state.suggestion_source = nil
  picker_ui.state.selected_files = {}
  picker_ui.state.selected_items = {}

  -- Write on_select into the config the picker reads during select().
  -- picker_ui.state.config is what the hook reads; set it directly so the
  -- test doesn't depend on picker_ui.open() lifecycle merging.
  local cfg = vim.deepcopy(conf.get())
  cfg.on_select = on_select
  picker_ui.state.config = cfg

  picker_ui.select(action)
end

describe('on_select hook', function()
  local sandbox_root, target_dir, target_filename

  before_each(function()
    sandbox_root = vim.fn.tempname()
    target_dir = sandbox_root .. '/on-select-dir'
    vim.fn.mkdir(target_dir, 'p')

    target_filename = 'on_select_fixture.lua'
    local fd = assert(io.open(target_dir .. '/' .. target_filename, 'w'))
    fd:write('-- on_select fixture\nreturn true\n')
    fd:close()

    pcall(vim.api.nvim_del_augroup_by_name, 'fff_file_tracking')

    vim.cmd('cd ' .. vim.fn.fnameescape(target_dir))
    vim.g.fff = {}
    file_picker.setup()

    assert.is_true(picker_ui.change_indexing_directory(target_dir))
    wait_for_scan(target_dir, 10000)
  end)

  after_each(function()
    pcall(picker_ui.close)
    pcall(fff_rust.stop_background_monitor)
    pcall(fff_rust.cleanup_file_picker)
    if sandbox_root then vim.fn.delete(sandbox_root, 'rf') end
    -- Reset to a scratch buffer so the next test starts clean.
    vim.cmd('enew!')
  end)

  local function get_items()
    local items = file_picker.search_files('', nil, nil, nil, nil)
    assert.is_true(#items > 0, 'indexer returned no items')
    return items
  end

  it('is called with selection table and action, receiving the picked item', function()
    local items = get_items()

    local captured = {}
    drive_select(items, target_filename, 'edit', function(sel, action)
      captured.selection = sel
      captured.action = action
      return true -- suppress default :edit so the buffer assertions below are about *our* handling
    end)

    assert.are.equal('edit', captured.action)
    assert.is_table(captured.selection)
    assert.is_string(captured.selection.path)
    assert.is_string(captured.selection.relative_path)
    assert.is_table(captured.selection.item)
    assert.are.equal(target_filename, captured.selection.item.name)
    -- path should resolve to the fixture on disk
    local expected = norm(target_dir .. '/' .. target_filename)
    assert.are.equal(expected, norm(captured.selection.path))
  end)

  it('returning true suppresses fff default :edit', function()
    local items = get_items()

    -- Switch to a known scratch buffer so we can verify fff did NOT replace it.
    vim.cmd('enew!')
    local before_buf = vim.api.nvim_get_current_buf()
    local before_name = vim.api.nvim_buf_get_name(before_buf)

    drive_select(items, target_filename, 'edit', function() return true end)

    local after_buf = vim.api.nvim_get_current_buf()
    local after_name = vim.api.nvim_buf_get_name(after_buf)
    assert.are.equal(before_buf, after_buf)
    assert.are.equal(before_name, after_name)
  end)

  it('returning nil falls through to fff default :edit', function()
    local items = get_items()

    vim.cmd('enew!')

    local hook_called = false
    drive_select(items, target_filename, 'edit', function()
      hook_called = true
      -- no explicit return
    end)

    assert.is_true(hook_called)
    local bufname = vim.api.nvim_buf_get_name(0)
    assert.is_true(bufname ~= '', 'expected fff default :edit to run and open a buffer')
    local expected = norm(target_dir .. '/' .. target_filename)
    assert.are.equal(expected, norm(bufname))
  end)

  it('returning false also falls through', function()
    local items = get_items()

    vim.cmd('enew!')

    drive_select(items, target_filename, 'edit', function() return false end)

    local bufname = vim.api.nvim_buf_get_name(0)
    local expected = norm(target_dir .. '/' .. target_filename)
    assert.are.equal(expected, norm(bufname))
  end)

  it('errors in the hook are caught and do not prevent fallback', function()
    local items = get_items()

    vim.cmd('enew!')

    -- Suppress the notify so busted output stays clean; just verify no crash
    -- and that fff still falls back to default handling.
    local orig_notify = vim.notify
    local notified = false
    vim.notify = function(_, _) notified = true end

    local ok = pcall(drive_select, items, target_filename, 'edit', function()
      error('boom')
    end)

    vim.notify = orig_notify

    assert.is_true(ok, 'select() should not propagate hook errors')
    assert.is_true(notified, 'fff should have reported the hook error via vim.notify')
    local bufname = vim.api.nvim_buf_get_name(0)
    local expected = norm(target_dir .. '/' .. target_filename)
    assert.are.equal(expected, norm(bufname))
  end)
end)
