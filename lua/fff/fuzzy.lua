---@class fff.fuzzy
local M = {}

-- Try to load the Rust module
local ok, rust_module = pcall(require, 'fff.rust')
if not ok then error('Failed to load fff.rust module: ' .. rust_module) end

--- Select the backend: daemon mode if available and configured, otherwise in-process.
--- When daemon mode is active, search requests go to the fff-daemon process
--- over a Unix domain socket, sharing a single index across all Neovim instances.
---@return table backend
local cached_backend = nil
local function select_backend()
  if cached_backend then return cached_backend end
  local conf = require('fff.conf')
  local config = conf.get()
  local use_daemon = config.daemon and config.daemon.enabled and rust_module.daemon_available and rust_module.daemon
  cached_backend = use_daemon and rust_module.daemon or rust_module
  return cached_backend
end

local function get_fn(name)
  return function(...)
    local backend = select_backend()
    local fn = backend[name]
    if fn then
      return fn(...)
    end
    return rust_module[name](...)
  end
end

-- Export all functions with daemon-aware dispatch
M.init_db = get_fn('init_db')
M.destroy_frecency_db = get_fn('destroy_frecency_db')
M.access = rust_module.access
M.set_provider_items = rust_module.set_provider_items
M.fuzzy = rust_module.fuzzy
M.fuzzy_matched_indices = rust_module.fuzzy_matched_indices
M.get_keyword_range = rust_module.get_keyword_range
M.guess_edit_range = rust_module.guess_edit_range
M.get_words = rust_module.get_words
M.init_file_picker = get_fn('init_file_picker')
M.restart_index_in_path = get_fn('restart_index_in_path')
M.scan_files = get_fn('scan_files')
M.get_cached_files = rust_module.get_cached_files
M.fuzzy_search_files = get_fn('fuzzy_search_files')
M.track_access = get_fn('track_access')
M.add_file = rust_module.add_file
M.remove_file = rust_module.remove_file
M.cancel_scan = get_fn('cancel_scan')
M.get_scan_progress = get_fn('get_scan_progress')
M.is_scanning = get_fn('is_scanning')
M.refresh_git_status = get_fn('refresh_git_status')
M.update_single_file_frecency = get_fn('update_single_file_frecency')
M.stop_background_monitor = get_fn('stop_background_monitor')
M.cleanup_file_picker = get_fn('cleanup_file_picker')
M.init_tracing = get_fn('init_tracing')
M.wait_for_initial_scan = get_fn('wait_for_initial_scan')

-- Query tracking functions
M.init_query_db = rust_module.init_query_db
M.destroy_query_db = get_fn('destroy_query_db')
M.track_query_completion = get_fn('track_query_completion')
M.get_historical_query = get_fn('get_historical_query')
M.track_grep_query = get_fn('track_grep_query')
M.get_historical_grep_query = get_fn('get_historical_grep_query')

-- Git functions
M.get_git_root = get_fn('get_git_root')

-- Grep functions
M.live_grep = get_fn('live_grep')
M.parse_grep_query = get_fn('parse_grep_query')

-- Utility functions
M.health_check = get_fn('health_check')
M.shorten_path = rust_module.shorten_path

return M
