local utils = require('fff.utils')

local M = {}

local function fetch_rust_checkhealth(rust_module, test_path)
  if not rust_module.health_check then
    return nil, 'health_check function not available in rust module (binary may be outdated)'
  end

  local ok, result = pcall(rust_module.health_check, test_path)
  if not ok then return nil, 'Failed to call health_check: ' .. tostring(result) end

  return result, nil
end

--- Check snacks.nvim image preview availability
--- @return table image_preview_info
local function check_image_preview()
  local ok, image = pcall(require, 'fff.file_picker.image')
  if not ok then
    return {
      available = false,
      snacks_available = false,
      snacks_image_available = false,
      terminal_supported = false,
      error = 'failed to load image module',
    }
  end

  return image.get_preview_status()
end

--- Check icon provider availability
--- @return table icon_provider_info
local function check_icon_provider()
  local ok, icons = pcall(require, 'fff.file_picker.icons')
  if not ok then return {
    available = false,
    name = nil,
    supports_directories = false,
  } end

  return icons.get_provider_info()
end

--- Run the health check and return structured results
--- @param opts? { test_path?: string } Options for health check
--- @return table health_result
function M.run(opts)
  opts = opts or {}

  local health = {
    ok = true,
    binary = {
      available = false,
      path = nil,
      error = nil,
    },
    rust = {
      version = nil,
      git = {
        available = false,
        repository_found = false,
        workdir = nil,
        libgit2_version = nil,
        error = nil,
      },
      file_picker = {
        initialized = false,
        base_path = nil,
        is_scanning = false,
        indexed_files = 0,
        error = nil,
      },
      frecency = {
        initialized = false,
        db_path = nil,
        disk_size = nil,
        entries = nil,
        error = nil,
      },
      query_tracker = {
        initialized = false,
        db_path = nil,
        disk_size = nil,
        query_file_entries = nil,
        query_history_entries = nil,
        error = nil,
      },
    },
    image_preview = {
      available = false,
      snacks_available = false,
      snacks_image_available = false,
      terminal_supported = false,
      error = nil,
    },
    icon_provider = {
      available = false,
      name = nil,
      supports_directories = false,
    },
    daemon = {
      feature_compiled = false,
      config_enabled = false,
      backend_active = false,
      binary_path = nil,
      process_running = false,
      pid = nil,
    },
    messages = {},
  }

  -- Check binary availability
  local download = require('fff.download')
  health.binary.path = download.get_binary_path()

  local binary_ok, rust_module = pcall(require, 'fff.rust')
  if not binary_ok then
    health.ok = false
    health.binary.available = false
    health.binary.error = tostring(rust_module)
    table.insert(health.messages, {
      level = 'error',
      msg = 'Binary not available: ' .. tostring(rust_module),
    })
    return health
  end

  health.binary.available = true
  table.insert(health.messages, {
    level = 'ok',
    msg = 'Binary loaded successfully from: ' .. health.binary.path,
  })

  -- Use the daemon-aware dispatch (fff.fuzzy) when daemon mode is active,
  -- otherwise fall back to the raw rust module.
  local conf = require('fff.conf')
  local config = conf.get()
  local use_daemon = config.daemon and config.daemon.enabled and rust_module.daemon_available and rust_module.daemon
  local health_fn_module = use_daemon and require('fff.fuzzy') or rust_module
  local rust_health, rust_err = fetch_rust_checkhealth(health_fn_module, opts.test_path)
  if rust_health then
    health.rust.version = rust_health.version
    table.insert(health.messages, {
      level = 'ok',
      msg = 'fff.nvim version: ' .. (rust_health.version or 'unknown'),
    })

    if rust_health.git then
      health.rust.git.available = rust_health.git.available
      health.rust.git.repository_found = rust_health.git.repository_found
      health.rust.git.workdir = rust_health.git.workdir
      health.rust.git.libgit2_version = rust_health.git.libgit2_version
      health.rust.git.error = rust_health.git.error

      if rust_health.git.available then
        table.insert(health.messages, {
          level = 'ok',
          msg = 'libgit2 available (version: ' .. (rust_health.git.libgit2_version or 'unknown') .. ')',
        })

        if rust_health.git.repository_found then
          table.insert(health.messages, {
            level = 'ok',
            msg = 'Git repository found: ' .. (rust_health.git.workdir or 'unknown'),
          })
        else
          table.insert(health.messages, {
            level = 'info',
            msg = 'No git repository found in current directory'
              .. (rust_health.git.error and (': ' .. rust_health.git.error) or ''),
          })
        end
      else
        table.insert(health.messages, {
          level = 'warn',
          msg = 'libgit2 not available',
        })
      end
    end

    if rust_health.file_picker then
      health.rust.file_picker.initialized = rust_health.file_picker.initialized
      health.rust.file_picker.base_path = rust_health.file_picker.base_path
      health.rust.file_picker.is_scanning = rust_health.file_picker.is_scanning
      health.rust.file_picker.indexed_files = rust_health.file_picker.indexed_files
      health.rust.file_picker.error = rust_health.file_picker.error

      if rust_health.file_picker.initialized then
        local status = rust_health.file_picker.is_scanning and 'scanning' or 'ready'
        table.insert(health.messages, {
          level = 'ok',
          msg = string.format(
            'File picker initialized (%s, %d files indexed, base: %s)',
            status,
            rust_health.file_picker.indexed_files or 0,
            rust_health.file_picker.base_path or 'unknown'
          ),
        })
      else
        table.insert(health.messages, {
          level = 'info',
          msg = 'File picker not initialized (will initialize on first use)',
        })
      end
    end

    -- Frecency database status
    if rust_health.frecency then
      health.rust.frecency.initialized = rust_health.frecency.initialized
      health.rust.frecency.error = rust_health.frecency.error

      if rust_health.frecency.initialized then
        local db_info = rust_health.frecency.db_healthcheck
        if db_info then
          health.rust.frecency.db_path = db_info.path
          health.rust.frecency.disk_size = db_info.disk_size
          health.rust.frecency.entries = db_info.absolute_frecency_entries

          table.insert(health.messages, {
            level = 'ok',
            msg = string.format(
              'Frecency database initialized (%d entries, %s, path: %s)',
              db_info.absolute_frecency_entries or 0,
              utils.format_file_size(db_info.disk_size or 0),
              db_info.path or 'unknown'
            ),
          })
        elseif rust_health.frecency.db_healthcheck_error then
          table.insert(health.messages, {
            level = 'warn',
            msg = 'Frecency database initialized but health check failed: '
              .. rust_health.frecency.db_healthcheck_error,
          })
        else
          table.insert(health.messages, {
            level = 'ok',
            msg = 'Frecency database initialized',
          })
        end
      else
        table.insert(health.messages, {
          level = 'info',
          msg = 'Frecency database not initialized (will initialize on first use)',
        })
      end
    end

    if rust_health.query_tracker then
      health.rust.query_tracker.initialized = rust_health.query_tracker.initialized
      health.rust.query_tracker.error = rust_health.query_tracker.error

      if rust_health.query_tracker.initialized then
        local db_info = rust_health.query_tracker.db_healthcheck
        if db_info then
          health.rust.query_tracker.db_path = db_info.path
          health.rust.query_tracker.disk_size = db_info.disk_size
          health.rust.query_tracker.query_file_entries = db_info.query_file_entries
          health.rust.query_tracker.query_history_entries = db_info.query_history_entries

          table.insert(health.messages, {
            level = 'ok',
            msg = string.format(
              'Query tracker initialized (%d query-file mappings, %d history entries, %s, path: %s)',
              db_info.query_file_entries or 0,
              db_info.query_history_entries or 0,
              utils.format_file_size(db_info.disk_size or 0),
              db_info.path or 'unknown'
            ),
          })
        elseif rust_health.query_tracker.db_healthcheck_error then
          table.insert(health.messages, {
            level = 'warn',
            msg = 'Query tracker initialized but health check failed: '
              .. rust_health.query_tracker.db_healthcheck_error,
          })
        else
          table.insert(health.messages, {
            level = 'ok',
            msg = 'Query tracker initialized',
          })
        end
      else
        table.insert(health.messages, {
          level = 'info',
          msg = 'Query tracker not initialized (will initialize on first use)',
        })
      end
    end
  else
    health.ok = false
    table.insert(health.messages, {
      level = 'error',
      msg = rust_err or 'Unknown error getting rust health data',
    })
    return health
  end

  -- Daemon mode status
  do
    local conf = require('fff.conf')
    local config = conf.get()
    local daemon_enabled = config.daemon and config.daemon.enabled
    health.daemon.config_enabled = daemon_enabled or false
    health.daemon.feature_compiled = rust_module.daemon_available or false

    -- Check daemon binary path
    local daemon_bin = vim.env.FFF_DAEMON_BIN
    health.daemon.binary_path = daemon_bin

    -- Check if daemon process is running
    if daemon_bin then
      local result = vim.system({ 'pgrep', '-f', 'fff-daemon' }, { text = true }):wait()
      if result.code == 0 and result.stdout and result.stdout ~= '' then
        health.daemon.process_running = true
        health.daemon.pid = vim.trim(vim.split(result.stdout, '\n')[1] or '')
      end
    end

    -- Check which backend fuzzy.lua selected
    local fuzzy_ok, fuzzy = pcall(require, 'fff.fuzzy')
    if fuzzy_ok and daemon_enabled and rust_module.daemon_available and rust_module.daemon then
      health.daemon.backend_active = true
    end

    -- Emit messages
    if health.daemon.feature_compiled then
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Daemon feature compiled into binary',
      })
    else
      table.insert(health.messages, {
        level = 'info',
        msg = 'Daemon feature not compiled (build with --features daemon to enable)',
      })
    end

    if daemon_enabled then
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Daemon mode enabled in config',
      })
    else
      table.insert(health.messages, {
        level = 'info',
        msg = 'Daemon mode disabled in config (set daemon.enabled = true to activate)',
      })
    end

    if daemon_bin then
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Daemon binary found: ' .. daemon_bin,
      })
    else
      if daemon_enabled then
        table.insert(health.messages, {
          level = 'warn',
          msg = 'Daemon binary not found (FFF_DAEMON_BIN not set — build fff-daemon or check installation)',
        })
      end
    end

    if health.daemon.process_running then
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Daemon process running (pid ' .. health.daemon.pid .. ')',
      })
    else
      if daemon_enabled and health.daemon.feature_compiled then
        table.insert(health.messages, {
          level = 'info',
          msg = 'Daemon process not running (will auto-start on first file picker use)',
        })
      end
    end

    if health.daemon.backend_active then
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Active backend: daemon',
      })

      -- Show per-directory info from the daemon
      local list_fn = rust_module.daemon and rust_module.daemon.list_directories
      if list_fn then
        local lok, dirs = pcall(list_fn)
        if lok and dirs then
          health.daemon.directories = dirs
          if #dirs == 0 then
            table.insert(health.messages, {
              level = 'info',
              msg = 'Daemon directory list is empty',
            })
          end
          for _, dir in ipairs(dirs) do
            local status = dir.is_scanning and 'scanning' or 'ready'
            local pid_suffix = ''
            if dir.client_pids and #dir.client_pids > 0 then
              pid_suffix = string.format(', pids: %s', table.concat(dir.client_pids, ', '))
            end
            table.insert(health.messages, {
              level = 'ok',
              msg = string.format(
                'Daemon directory: %s (%s, %d files, %d client%s%s)',
                dir.path,
                status,
                dir.file_count,
                dir.client_count,
                dir.client_count == 1 and '' or 's',
                pid_suffix
              ),
            })
          end
        else
          table.insert(health.messages, {
            level = 'warn',
            msg = 'Failed to query daemon directories: ' .. tostring(dirs),
          })
        end
      else
        table.insert(health.messages, {
          level = 'warn',
          msg = 'Daemon backend missing list_directories() export',
        })
      end
    else
      table.insert(health.messages, {
        level = 'ok',
        msg = 'Active backend: in-process',
      })
    end
  end

  local image_info = check_image_preview()
  health.image_preview.snacks_available = image_info.snacks_available
  health.image_preview.snacks_image_available = image_info.snacks_image_available
  health.image_preview.terminal_supported = image_info.terminal_supported
  health.image_preview.error = image_info.error
  health.image_preview.available = image_info.available

  if image_info.available then
    table.insert(health.messages, {
      level = 'ok',
      msg = 'Image preview available via snacks.nvim',
    })
  elseif image_info.snacks_available and image_info.snacks_image_available then
    table.insert(health.messages, {
      level = 'info',
      msg = 'Image preview not available: ' .. (image_info.error or 'terminal does not support images'),
    })
  elseif image_info.snacks_available then
    table.insert(health.messages, {
      level = 'info',
      msg = 'Image preview not available: snacks.image module not found',
    })
  else
    table.insert(health.messages, {
      level = 'info',
      msg = 'Image preview not available: snacks.nvim not installed',
    })
  end

  local icon_info = check_icon_provider()
  health.icon_provider.available = icon_info.available
  health.icon_provider.name = icon_info.name
  health.icon_provider.supports_directories = icon_info.supports_directories

  if icon_info.available then
    table.insert(health.messages, {
      level = 'ok',
      msg = 'Filetype icons available via ' .. icon_info.name,
    })
  else
    table.insert(health.messages, {
      level = 'info',
      msg = 'Filetype icons not available (install nvim-web-devicons or mini.icons)',
    })
  end

  return health
end

function M.check()
  vim.health.start('fff.nvim')

  local result = M.run()

  for _, msg in ipairs(result.messages) do
    if msg.level == 'ok' then
      vim.health.ok(msg.msg)
    elseif msg.level == 'warn' then
      vim.health.warn(msg.msg)
    elseif msg.level == 'error' then
      vim.health.error(msg.msg)
    elseif msg.level == 'info' then
      vim.health.info(msg.msg)
    end
  end

  if not result.binary.available then
    vim.health.info('To install the binary, run:')
    vim.health.info('  :lua require("fff.download").download_or_build_binary()')
    vim.health.info('Or build from source with:')
    vim.health.info('  cargo build --release')
  end
end

return M
