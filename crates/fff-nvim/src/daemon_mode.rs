use fff_client::FffClient;
use fff_protocol::*;
use mlua::prelude::*;
use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::Mutex;

static DAEMON_CLIENT: Lazy<Mutex<Option<FffClient>>> = Lazy::new(|| Mutex::new(None));
/// `init_db` args received before `init_file_picker` has connected a client.
/// Replayed on the first connection, and stored inside the client so
/// subsequent reconnects also replay them.
static PENDING_INIT_DB: Lazy<Mutex<Option<(String, String, bool)>>> =
    Lazy::new(|| Mutex::new(None));

fn with_client<F, R>(f: F) -> LuaResult<R>
where
    F: FnOnce(&mut FffClient) -> fff_client::Result<R>,
{
    let mut guard = DAEMON_CLIENT.lock().map_err(|e| {
        LuaError::RuntimeError(format!("failed to lock daemon client: {e}"))
    })?;
    let client = guard.as_mut().ok_or_else(|| {
        LuaError::RuntimeError("daemon client not initialized — call init_file_picker first".into())
    })?;
    f(client).map_err(|e| LuaError::RuntimeError(format!("daemon error: {e}")))
}

pub fn init_db(
    _: &Lua,
    (frecency_db_path, history_db_path, use_unsafe_no_lock): (String, String, bool),
) -> LuaResult<bool> {
    // If client is already connected, forward the init_db call immediately
    // so the daemon has the DB paths and the client remembers them for
    // replay after reconnect.
    let mut guard = DAEMON_CLIENT.lock().map_err(|e| {
        LuaError::RuntimeError(format!("failed to lock daemon client: {e}"))
    })?;
    if let Some(ref mut client) = *guard {
        client
            .init_db(&frecency_db_path, &history_db_path, use_unsafe_no_lock)
            .map_err(|e| LuaError::RuntimeError(format!("daemon error: {e}")))?;
        return Ok(true);
    }

    // No client yet — remember init_db args by connecting lazily.
    // We can't actually connect without a base_path, so stash the args in a
    // module-local slot that init_file_picker will replay once it connects.
    *PENDING_INIT_DB.lock().map_err(|e| {
        LuaError::RuntimeError(format!("failed to lock pending init_db: {e}"))
    })? = Some((frecency_db_path, history_db_path, use_unsafe_no_lock));
    Ok(true)
}

pub fn init_file_picker(
    _: &Lua,
    (base_path, watch_git_events): (String, Option<bool>),
) -> LuaResult<bool> {
    let mut guard = DAEMON_CLIENT.lock().map_err(|e| {
        LuaError::RuntimeError(format!("failed to lock daemon client: {e}"))
    })?;

    if let Some(ref mut client) = *guard {
        client
            .init_file_picker(&base_path, watch_git_events.unwrap_or(true))
            .map_err(|e| LuaError::RuntimeError(format!("daemon error: {e}")))?;
        return Ok(true);
    }

    let mut client = FffClient::connect(Path::new(&base_path)).map_err(|e| {
        LuaError::RuntimeError(format!("failed to connect to daemon: {e}"))
    })?;

    // Replay any init_db that was queued before init_file_picker so the
    // daemon learns about the DB paths, and so reconnect-replay has it.
    if let Some((frecency, history, use_unsafe)) = PENDING_INIT_DB
        .lock()
        .map_err(|e| LuaError::RuntimeError(format!("failed to lock pending init_db: {e}")))?
        .take()
    {
        client
            .init_db(&frecency, &history, use_unsafe)
            .map_err(|e| LuaError::RuntimeError(format!("daemon error: {e}")))?;
    }

    client
        .init_file_picker(&base_path, watch_git_events.unwrap_or(true))
        .map_err(|e| LuaError::RuntimeError(format!("daemon error: {e}")))?;

    *guard = Some(client);
    Ok(true)
}

pub fn restart_index_in_path(_: &Lua, new_path: String) -> LuaResult<()> {
    with_client(|c| {
        c.restart_index_in_path(&new_path)?;
        Ok(())
    })
}

pub fn scan_files(_: &Lua, _: ()) -> LuaResult<()> {
    with_client(|c| c.scan_files())
}

pub fn fuzzy_search_files(
    lua: &Lua,
    (query, max_threads, current_file, combo_boost_score_multiplier, min_combo_count, page_index, page_size): (
        String,
        usize,
        Option<String>,
        i32,
        Option<u32>,
        Option<usize>,
        Option<usize>,
    ),
) -> LuaResult<LuaValue> {
    let result = with_client(|c| {
        c.fuzzy_search_files(
            &query,
            max_threads,
            current_file.as_deref(),
            combo_boost_score_multiplier,
            min_combo_count.unwrap_or(3),
            page_index.unwrap_or(0),
            page_size.unwrap_or(0),
        )
    })?;

    search_result_wire_to_lua(lua, &result)
}

pub fn live_grep(
    lua: &Lua,
    (query, file_offset, page_size, max_file_size, max_matches_per_file, smart_case, grep_mode, time_budget_ms, trim_whitespace): (
        String,
        Option<usize>,
        Option<usize>,
        Option<u64>,
        Option<usize>,
        Option<bool>,
        Option<String>,
        Option<u64>,
        Option<bool>,
    ),
) -> LuaResult<LuaValue> {
    let mode = match grep_mode.as_deref() {
        Some("regex") => GrepModeWire::Regex,
        Some("fuzzy") => GrepModeWire::Fuzzy,
        _ => GrepModeWire::PlainText,
    };

    let result = with_client(|c| {
        c.live_grep(
            &query,
            file_offset.unwrap_or(0),
            page_size.unwrap_or(50),
            max_file_size.unwrap_or(10 * 1024 * 1024),
            max_matches_per_file.unwrap_or(200),
            smart_case.unwrap_or(true),
            mode,
            time_budget_ms.unwrap_or(0),
            trim_whitespace.unwrap_or(false),
        )
    })?;

    grep_result_wire_to_lua(lua, &result)
}

pub fn track_access(_: &Lua, file_path: String) -> LuaResult<bool> {
    with_client(|c| c.track_access(&file_path))
}

pub fn get_scan_progress(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    let progress = with_client(|c| c.get_scan_progress())?;
    let table = lua.create_table()?;
    table.set("scanned_files_count", progress.scanned_files_count)?;
    table.set("is_scanning", progress.is_scanning)?;
    Ok(LuaValue::Table(table))
}

pub fn is_scanning(_: &Lua, _: ()) -> LuaResult<bool> {
    with_client(|c| c.is_scanning())
}

pub fn get_git_root(_: &Lua, _: ()) -> LuaResult<Option<String>> {
    with_client(|c| c.get_git_root())
}

pub fn get_base_path(_: &Lua, _: ()) -> LuaResult<Option<String>> {
    with_client(|c| c.get_base_path())
}

pub fn refresh_git_status(_: &Lua, _: ()) -> LuaResult<usize> {
    with_client(|c| c.refresh_git_status())
}

pub fn update_single_file_frecency(_: &Lua, file_path: String) -> LuaResult<bool> {
    with_client(|c| c.update_single_file_frecency(&file_path))
}

pub fn stop_background_monitor(_: &Lua, _: ()) -> LuaResult<bool> {
    with_client(|c| c.stop_background_monitor())
}

pub fn cleanup_file_picker(_: &Lua, _: ()) -> LuaResult<bool> {
    with_client(|c| c.cleanup_file_picker())
}

pub fn wait_for_initial_scan(_: &Lua, timeout_ms: Option<u64>) -> LuaResult<bool> {
    with_client(|c| c.wait_for_initial_scan(timeout_ms.unwrap_or(500)))
}

pub fn track_query_completion(_: &Lua, (query, file_path): (String, String)) -> LuaResult<bool> {
    with_client(|c| c.track_query_completion(&query, &file_path))
}

pub fn get_historical_query(_: &Lua, offset: usize) -> LuaResult<Option<String>> {
    with_client(|c| c.get_historical_query(offset))
}

pub fn track_grep_query(_: &Lua, query: String) -> LuaResult<bool> {
    with_client(|c| c.track_grep_query(&query))
}

pub fn get_historical_grep_query(_: &Lua, offset: usize) -> LuaResult<Option<String>> {
    with_client(|c| c.get_historical_grep_query(offset))
}

pub fn parse_grep_query(lua: &Lua, query: String) -> LuaResult<LuaTable> {
    let grep_text = with_client(|c| c.parse_grep_query(&query))?;
    let table = lua.create_table()?;
    table.set("grep_text", grep_text)?;
    Ok(table)
}

pub fn health_check(lua: &Lua, _test_path: Option<String>) -> LuaResult<LuaValue> {
    let health = with_client(|c| c.health_check())?;

    let table = lua.create_table()?;
    table.set("version", health.version.as_str())?;

    let git_info = lua.create_table()?;
    git_info.set("available", health.git_available)?;
    git_info.set("repository_found", health.git_repository_found)?;
    git_info.set("libgit2_version", health.git_libgit2_version.as_str())?;
    if let Some(ref workdir) = health.git_workdir {
        git_info.set("workdir", workdir.as_str())?;
    }
    table.set("git", git_info)?;

    let picker_info = lua.create_table()?;
    picker_info.set("initialized", health.file_picker_initialized)?;
    if let Some(ref bp) = health.file_picker_base_path {
        picker_info.set("base_path", bp.as_str())?;
    }
    if let Some(scanning) = health.file_picker_is_scanning {
        picker_info.set("is_scanning", scanning)?;
    }
    if let Some(files) = health.file_picker_indexed_files {
        picker_info.set("indexed_files", files)?;
    }
    table.set("file_picker", picker_info)?;

    let frecency_info = lua.create_table()?;
    frecency_info.set("initialized", true)?;
    table.set("frecency", frecency_info)?;

    let qt_info = lua.create_table()?;
    qt_info.set("initialized", true)?;
    table.set("query_tracker", qt_info)?;

    Ok(LuaValue::Table(table))
}

pub fn list_directories(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    let dirs = with_client(|c| c.list_directories())?;
    let table = lua.create_table()?;
    for (i, dir) in dirs.iter().enumerate() {
        let dt = lua.create_table()?;
        dt.set("path", dir.path.to_string_lossy().as_ref())?;
        dt.set("is_scanning", dir.is_scanning)?;
        dt.set("file_count", dir.file_count)?;
        dt.set("client_count", dir.client_count)?;
        let pids = lua.create_table()?;
        for (j, pid) in dir.client_pids.iter().enumerate() {
            pids.set(j + 1, *pid)?;
        }
        dt.set("client_pids", pids)?;
        table.set(i + 1, dt)?;
    }
    Ok(LuaValue::Table(table))
}

pub fn cancel_scan(_: &Lua, _: ()) -> LuaResult<bool> {
    Ok(true)
}

pub fn destroy_frecency_db(_: &Lua, _: ()) -> LuaResult<bool> {
    tracing::warn!("destroy_frecency_db not supported in daemon mode");
    Ok(false)
}

pub fn destroy_query_db(_: &Lua, _: ()) -> LuaResult<bool> {
    tracing::warn!("destroy_query_db not supported in daemon mode");
    Ok(false)
}

// -- Lua table conversion helpers --

fn search_result_wire_to_lua(lua: &Lua, result: &SearchResultWire) -> LuaResult<LuaValue> {
    let table = lua.create_table()?;

    let items_table = lua.create_table()?;
    for (i, item) in result.items.iter().enumerate() {
        let it = lua.create_table()?;
        it.set("relative_path", item.relative_path.as_str())?;
        it.set("name", item.name.as_str())?;
        it.set("size", item.size)?;
        it.set("modified", item.modified)?;
        it.set("access_frecency_score", item.access_frecency_score as i32)?;
        it.set("modification_frecency_score", item.modification_frecency_score as i32)?;
        it.set("total_frecency_score", item.total_frecency_score)?;
        it.set("git_status", item.git_status.as_str())?;
        it.set("is_binary", item.is_binary)?;
        items_table.set(i + 1, it)?;
    }
    table.set("items", items_table)?;

    let scores_table = lua.create_table()?;
    for (i, score) in result.scores.iter().enumerate() {
        let st = lua.create_table()?;
        st.set("total", score.total)?;
        st.set("base_score", score.base_score)?;
        st.set("filename_bonus", score.filename_bonus)?;
        st.set("special_filename_bonus", score.special_filename_bonus)?;
        st.set("frecency_boost", score.frecency_boost)?;
        st.set("git_status_boost", score.git_status_boost)?;
        st.set("distance_penalty", score.distance_penalty)?;
        st.set("current_file_penalty", score.current_file_penalty)?;
        st.set("combo_match_boost", score.combo_match_boost)?;
        st.set("path_alignment_bonus", score.path_alignment_bonus)?;
        st.set("exact_match", score.exact_match)?;
        st.set("match_type", score.match_type.as_str())?;
        scores_table.set(i + 1, st)?;
    }
    table.set("scores", scores_table)?;

    table.set("total_matched", result.total_matched)?;
    table.set("total_files", result.total_files)?;

    if let Some(ref loc) = result.location {
        let loc_table = lua.create_table()?;
        match loc {
            LocationWire::Line(l) => {
                loc_table.set("line", *l)?;
            }
            LocationWire::Position { line, col } => {
                loc_table.set("line", *line)?;
                loc_table.set("col", *col)?;
            }
            LocationWire::Range { start, end } => {
                let start_t = lua.create_table()?;
                start_t.set("line", start.0)?;
                start_t.set("col", start.1)?;
                let end_t = lua.create_table()?;
                end_t.set("line", end.0)?;
                end_t.set("col", end.1)?;
                loc_table.set("start", start_t)?;
                loc_table.set("end", end_t)?;
            }
        }
        table.set("location", loc_table)?;
    }

    Ok(LuaValue::Table(table))
}

fn grep_result_wire_to_lua(lua: &Lua, result: &GrepResultWire) -> LuaResult<LuaValue> {
    let table = lua.create_table()?;

    let items_table = lua.create_table()?;
    for (i, item) in result.items.iter().enumerate() {
        let it = lua.create_table()?;
        it.set("relative_path", item.relative_path.as_str())?;
        it.set("name", item.name.as_str())?;
        it.set("is_binary", item.is_binary)?;
        it.set("git_status", item.git_status.as_str())?;
        it.set("size", item.size)?;
        it.set("modified", item.modified)?;
        it.set("total_frecency_score", item.total_frecency_score)?;
        it.set("access_frecency_score", item.access_frecency_score as i32)?;
        it.set("modification_frecency_score", item.modification_frecency_score as i32)?;
        it.set("line_number", item.line_number)?;
        it.set("col", item.col)?;
        it.set("byte_offset", item.byte_offset)?;
        it.set("line_content", item.line_content.as_str())?;

        let ranges = lua.create_table()?;
        for (j, (start, end)) in item.match_ranges.iter().enumerate() {
            let range = lua.create_table()?;
            range.set(1, *start)?;
            range.set(2, *end)?;
            ranges.set(j + 1, range)?;
        }
        it.set("match_ranges", ranges)?;

        if let Some(fs) = item.fuzzy_score {
            it.set("fuzzy_score", fs)?;
        }

        items_table.set(i + 1, it)?;
    }
    table.set("items", items_table)?;

    table.set("total_matched", result.total_matched)?;
    table.set("total_files_searched", result.total_files_searched)?;
    table.set("total_files", result.total_files)?;
    table.set("filtered_file_count", result.filtered_file_count)?;
    table.set("next_file_offset", result.next_file_offset)?;

    if let Some(ref err) = result.regex_fallback_error {
        table.set("regex_fallback_error", err.as_str())?;
    }

    Ok(LuaValue::Table(table))
}
