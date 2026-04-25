use std::sync::Arc;
use std::time::Duration;

use fff::grep::GrepSearchOptions;
use fff::types::PaginationArgs;
use fff::FuzzySearchOptions;
use fff_protocol::*;
use git2;
use tokio::sync::RwLock;

use crate::state::DaemonState;

pub async fn handle_request(
    state: Arc<RwLock<DaemonState>>,
    request: Request,
    client_pid: Option<u32>,
) -> Response {
    match request {
        Request::Ping => Response::Pong,

        Request::Shutdown => {
            tracing::info!("shutdown requested");
            Response::Ok
        }

        Request::InitDb {
            frecency_db_path,
            history_db_path,
            use_unsafe_no_lock,
        } => {
            let mut state = state.write().await;
            state.init_db(frecency_db_path, history_db_path, use_unsafe_no_lock);
            Response::Bool(true)
        }

        Request::IndexDirectory {
            path,
            watch_git_events,
        } => {
            let mut state = state.write().await;
            match state.ensure_directory(&path, watch_git_events, client_pid) {
                Ok(_) => Response::Bool(true),
                Err(e) => Response::Error(e),
            }
        }

        Request::DropDirectory { path } => {
            let mut state = state.write().await;
            Response::Bool(state.drop_directory(&path))
        }

        Request::ListDirectories => {
            let state = state.read().await;
            Response::Directories(state.list_directories())
        }

        Request::WaitForScan { path, timeout_ms } => {
            let scan_signal = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(e) => return Response::Error(e),
                }
            };
            let timeout = Duration::from_millis(timeout_ms);
            let completed = scan_signal.wait_for_scan(timeout);
            Response::Bool(completed)
        }

        Request::GetScanProgress { path } => {
            let picker_handle = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(e) => return Response::Error(e),
                }
            };
            let result = match picker_handle.read() {
                Ok(guard) => match guard.as_ref() {
                    Some(picker) => {
                        let progress = picker.get_scan_progress();
                        Response::ScanProgress(ScanProgressWire {
                            scanned_files_count: progress.scanned_files_count,
                            is_scanning: progress.is_scanning,
                        })
                    }
                    None => Response::Error("picker not initialized".into()),
                },
                Err(e) => Response::Error(format!("{e}")),
            };
            result
        }

        Request::IsScanning { path } => {
            let picker_handle = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(e) => return Response::Error(e),
                }
            };
            let result = match picker_handle.read() {
                Ok(guard) => match guard.as_ref() {
                    Some(picker) => Response::Bool(picker.is_scan_active()),
                    None => Response::Bool(false),
                },
                Err(e) => Response::Error(format!("{e}")),
            };
            result
        }

        Request::FuzzySearch {
            path,
            query,
            max_threads,
            current_file,
            combo_boost_score_multiplier,
            min_combo_count,
            page_index,
            page_size,
        } => {
            let (picker_handle, _, qt_handle) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            // Run search on blocking thread to avoid holding the async runtime
            let result = tokio::task::spawn_blocking(move || {
                let picker_guard = picker_handle.read().map_err(|e| format!("{e}"))?;
                let picker = picker_guard
                    .as_ref()
                    .ok_or_else(|| "picker not initialized".to_string())?;
                let qt_guard = qt_handle.read().map_err(|e| format!("{e}"))?;

                let parser =
                    fff_query_parser::QueryParser::new(fff_query_parser::FileSearchConfig);
                let parsed = parser.parse(&query);

                let result = picker.fuzzy_search(
                    &parsed,
                    qt_guard.as_ref(),
                    FuzzySearchOptions {
                        max_threads,
                        current_file: current_file.as_deref(),
                        project_path: Some(picker.base_path()),
                        combo_boost_score_multiplier,
                        min_combo_count,
                        pagination: PaginationArgs {
                            offset: page_index,
                            limit: page_size,
                        },
                    },
                );

                let items: Vec<FileItemWire> = result
                    .items
                    .iter()
                    .map(|fi| file_item_to_wire(fi, picker))
                    .collect();

                let scores: Vec<ScoreWire> =
                    result.scores.iter().map(score_to_wire).collect();

                let location = result.location.map(location_to_wire);

                Ok::<_, String>(SearchResultWire {
                    items,
                    scores,
                    total_matched: result.total_matched,
                    total_files: result.total_files,
                    location,
                })
            })
            .await;

            match result {
                Ok(Ok(wire)) => Response::SearchResult(wire),
                Ok(Err(e)) => Response::Error(e),
                Err(e) => Response::Error(format!("task join error: {e}")),
            }
        }

        Request::GrepSearch {
            path,
            query,
            file_offset,
            page_size,
            max_file_size,
            max_matches_per_file,
            smart_case,
            grep_mode,
            time_budget_ms,
            trim_whitespace,
        } => {
            let (picker_handle, _, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let mode = match grep_mode {
                GrepModeWire::PlainText => fff::GrepMode::PlainText,
                GrepModeWire::Regex => fff::GrepMode::Regex,
                GrepModeWire::Fuzzy => fff::GrepMode::Fuzzy,
            };

            let result = tokio::task::spawn_blocking(move || {
                let picker_guard = picker_handle.read().map_err(|e| format!("{e}"))?;
                let picker = picker_guard
                    .as_ref()
                    .ok_or_else(|| "picker not initialized".to_string())?;

                let parsed = fff::parse_grep_query(&query);

                let grep_result = picker.grep(
                    &parsed,
                    &GrepSearchOptions {
                        max_file_size,
                        max_matches_per_file,
                        smart_case,
                        file_offset,
                        page_limit: page_size,
                        mode,
                        time_budget_ms,
                        trim_whitespace,
                        ..Default::default()
                    },
                );

                let items: Vec<GrepMatchWire> = grep_result
                    .matches
                    .iter()
                    .map(|gm| {
                        let file = &grep_result.files[gm.file_index];
                        grep_match_to_wire(gm, file, picker)
                    })
                    .collect();

                Ok::<_, String>(GrepResultWire {
                    items,
                    total_matched: grep_result.matches.len(),
                    total_files_searched: grep_result.total_files_searched,
                    total_files: grep_result.total_files,
                    filtered_file_count: grep_result.filtered_file_count,
                    next_file_offset: grep_result.next_file_offset,
                    regex_fallback_error: grep_result.regex_fallback_error,
                })
            })
            .await;

            match result {
                Ok(Ok(wire)) => Response::GrepResult(wire),
                Ok(Err(e)) => Response::Error(e),
                Err(e) => Response::Error(format!("task join error: {e}")),
            }
        }

        Request::TrackAccess { path, file_path } => {
            let (picker_handle, frecency_handle, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            tokio::task::spawn_blocking(move || {
                if let Ok(guard) = frecency_handle.read() {
                    if let Some(frecency) = guard.as_ref() {
                        let _ = frecency.track_access(std::path::Path::new(&file_path));
                    }
                }
                if let Ok(mut guard) = picker_handle.write() {
                    if let Some(picker) = guard.as_mut() {
                        if let Ok(fg) = frecency_handle.read() {
                            if let Some(frecency) = fg.as_ref() {
                                let _ = picker.update_single_file_frecency(&file_path, frecency);
                            }
                        }
                    }
                }
            })
            .await
            .ok();

            Response::Bool(true)
        }

        Request::UpdateSingleFileFrecency { path, file_path } => {
            let (picker_handle, frecency_handle, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            tokio::task::spawn_blocking(move || {
                if let Ok(fg) = frecency_handle.read() {
                    if let Some(frecency) = fg.as_ref() {
                        if let Ok(mut pg) = picker_handle.write() {
                            if let Some(picker) = pg.as_mut() {
                                let _ = picker.update_single_file_frecency(&file_path, frecency);
                            }
                        }
                    }
                }
            })
            .await
            .ok();

            Response::Bool(true)
        }

        Request::TrackQueryCompletion {
            path,
            query,
            file_path,
        } => {
            let (picker_handle, _, qt_handle) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            tokio::task::spawn_blocking(move || {
                let project_path = picker_handle
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|p| p.base_path().to_path_buf()));
                if let Some(project_path) = project_path {
                    if let Ok(canonical) = fff::path_utils::canonicalize(&file_path) {
                        if let Ok(mut guard) = qt_handle.write() {
                            if let Some(tracker) = guard.as_mut() {
                                let _ = tracker.track_query_completion(
                                    &query,
                                    &project_path,
                                    &canonical,
                                );
                            }
                        }
                    }
                }
            })
            .await
            .ok();

            Response::Bool(true)
        }

        Request::TrackGrepQuery { path, query } => {
            let (picker_handle, _, qt_handle) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            tokio::task::spawn_blocking(move || {
                let project_path = picker_handle
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|p| p.base_path().to_path_buf()));
                if let Some(project_path) = project_path {
                    if let Ok(mut guard) = qt_handle.write() {
                        if let Some(tracker) = guard.as_mut() {
                            let _ = tracker.track_grep_query(&query, &project_path);
                        }
                    }
                }
            })
            .await
            .ok();

            Response::Bool(true)
        }

        Request::GetHistoricalQuery { path, offset } => {
            let (picker_handle, _, qt_handle) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                let project_path = picker_handle
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|p| p.base_path().to_path_buf()));
                project_path.and_then(|pp| {
                    qt_handle
                        .read()
                        .ok()
                        .and_then(|g| g.as_ref().and_then(|qt| qt.get_historical_query(&pp, offset).ok().flatten()))
                })
            })
            .await;

            match result {
                Ok(val) => Response::OptionalString(val),
                Err(e) => Response::Error(format!("{e}")),
            }
        }

        Request::GetHistoricalGrepQuery { path, offset } => {
            let (picker_handle, _, qt_handle) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                let project_path = picker_handle
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|p| p.base_path().to_path_buf()));
                project_path.and_then(|pp| {
                    qt_handle
                        .read()
                        .ok()
                        .and_then(|g| {
                            g.as_ref()
                                .and_then(|qt| qt.get_historical_grep_query(&pp, offset).ok().flatten())
                        })
                })
            })
            .await;

            match result {
                Ok(val) => Response::OptionalString(val),
                Err(e) => Response::Error(format!("{e}")),
            }
        }

        Request::RefreshGitStatus { path } => {
            let (picker_handle, frecency_handle, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                picker_handle.refresh_git_status(&frecency_handle)
            })
            .await;

            match result {
                Ok(Ok(count)) => Response::Usize(count),
                Ok(Err(e)) => Response::Error(format!("{e}")),
                Err(e) => Response::Error(format!("{e}")),
            }
        }

        Request::GetGitRoot { path } => {
            let picker_handle = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(e) => return Response::Error(e),
                }
            };
            let result = picker_handle
                .read()
                .ok()
                .and_then(|g| {
                    g.as_ref()
                        .and_then(|p| p.git_root().map(|r| r.to_string_lossy().into_owned()))
                });
            Response::OptionalString(result)
        }

        Request::GetBasePath { path } => {
            let picker_handle = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(e) => return Response::Error(e),
                }
            };
            let result = picker_handle
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|p| p.base_path().to_string_lossy().into_owned()));
            Response::OptionalString(result)
        }

        Request::TriggerRescan { path } => {
            let (picker_handle, frecency_handle, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                let mut guard = picker_handle.write().map_err(|e| format!("{e}"))?;
                if let Some(picker) = guard.as_mut() {
                    let _ = picker.trigger_rescan(&frecency_handle);
                }
                Ok::<_, String>(())
            })
            .await;

            match result {
                Ok(Ok(())) => Response::Ok,
                Ok(Err(e)) => Response::Error(e),
                Err(e) => Response::Error(format!("{e}")),
            }
        }

        Request::RestartIndex { path, new_path } => {
            let mut state = state.write().await;
            let watch_git = state
                .get_directory(&path)
                .map(|_| true)
                .unwrap_or(true);
            let _ = state.release_client(&path, client_pid);
            match state.ensure_directory(&new_path, watch_git, client_pid) {
                Ok(_) => Response::Ok,
                Err(e) => Response::Error(e),
            }
        }

        Request::StopBackgroundMonitor { path } => {
            let (picker_handle, _, _) = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok(h) => h,
                    Err(e) => return Response::Error(e),
                }
            };

            let result = tokio::task::spawn_blocking(move || {
                let mut guard = picker_handle.write().map_err(|e| format!("{e}"))?;
                if let Some(picker) = guard.as_mut() {
                    picker.stop_background_monitor();
                }
                Ok::<_, String>(())
            })
            .await;

            match result {
                Ok(Ok(())) => Response::Bool(true),
                Ok(Err(e)) => Response::Error(e),
                Err(e) => Response::Error(format!("{e}")),
            }
        }

        Request::CleanupFilePicker { path } => {
            let mut state = state.write().await;
            Response::Bool(state.release_client(&path, client_pid))
        }

        Request::DestroyFrecencyDb | Request::DestroyQueryDb => {
            Response::Error("not supported in daemon mode — manage DB lifecycle via the daemon's directory management".into())
        }

        Request::ParseGrepQuery { query } => {
            let parsed = fff::parse_grep_query(&query);
            Response::ParsedGrepQuery {
                grep_text: parsed.grep_text().to_string(),
            }
        }

        Request::HealthCheck { path } => {
            let picker_handle = {
                let state_guard = state.read().await;
                match state_guard.get_handles(&path) {
                    Ok((p, _, _)) => p,
                    Err(_) => {
                        return Response::HealthCheck(HealthCheckWire {
                            version: env!("CARGO_PKG_VERSION").to_string(),
                            git_available: false,
                            git_repository_found: false,
                            git_libgit2_version: String::new(),
                            git_workdir: None,
                            file_picker_initialized: false,
                            file_picker_base_path: None,
                            file_picker_is_scanning: None,
                            file_picker_indexed_files: None,
                        });
                    }
                }
            };

            let git_version = git2::Version::get();
            let (major, minor, rev) = git_version.libgit2_version();
            let git_version_str = format!("{major}.{minor}.{rev}");
            let git_info = git2::Repository::discover(&path).ok();

            let (initialized, base_path, is_scanning, indexed_files) = picker_handle
                .read()
                .ok()
                .and_then(|g| {
                    g.as_ref().map(|p| {
                        let progress = p.get_scan_progress();
                        (
                            true,
                            Some(p.base_path().to_string_lossy().into_owned()),
                            Some(progress.is_scanning),
                            Some(progress.scanned_files_count),
                        )
                    })
                })
                .unwrap_or((false, None, None, None));

            Response::HealthCheck(HealthCheckWire {
                version: env!("CARGO_PKG_VERSION").to_string(),
                git_available: true,
                git_repository_found: git_info.is_some(),
                git_libgit2_version: git_version_str,
                git_workdir: git_info.and_then(|r| {
                    r.workdir().map(|w| w.to_string_lossy().into_owned())
                }),
                file_picker_initialized: initialized,
                file_picker_base_path: base_path,
                file_picker_is_scanning: is_scanning,
                file_picker_indexed_files: indexed_files,
            })
        }
    }
}

fn file_item_to_wire(
    fi: &fff::types::FileItem,
    picker: &fff::file_picker::FilePicker,
) -> FileItemWire {
    FileItemWire {
        relative_path: fi.relative_path(picker),
        name: fi.file_name(picker),
        size: fi.size,
        modified: fi.modified,
        access_frecency_score: fi.access_frecency_score,
        modification_frecency_score: fi.modification_frecency_score,
        total_frecency_score: fi.total_frecency_score(),
        git_status: fff::git::format_git_status(fi.git_status).to_string(),
        is_binary: fi.is_binary(),
    }
}

fn grep_match_to_wire(
    gm: &fff::GrepMatch,
    file: &fff::types::FileItem,
    picker: &fff::file_picker::FilePicker,
) -> GrepMatchWire {
    GrepMatchWire {
        relative_path: file.relative_path(picker),
        name: file.file_name(picker),
        is_binary: file.is_binary(),
        git_status: fff::git::format_git_status(file.git_status).to_string(),
        size: file.size,
        modified: file.modified,
        total_frecency_score: file.total_frecency_score(),
        access_frecency_score: file.access_frecency_score,
        modification_frecency_score: file.modification_frecency_score,
        line_number: gm.line_number,
        col: gm.col,
        byte_offset: gm.byte_offset,
        line_content: gm.line_content.clone(),
        match_ranges: gm.match_byte_offsets.iter().copied().collect(),
        fuzzy_score: gm.fuzzy_score,
    }
}

fn score_to_wire(score: &fff::types::Score) -> ScoreWire {
    ScoreWire {
        total: score.total,
        base_score: score.base_score,
        filename_bonus: score.filename_bonus,
        special_filename_bonus: score.special_filename_bonus,
        frecency_boost: score.frecency_boost,
        git_status_boost: score.git_status_boost,
        distance_penalty: score.distance_penalty,
        current_file_penalty: score.current_file_penalty,
        combo_match_boost: score.combo_match_boost,
        path_alignment_bonus: score.path_alignment_bonus,
        exact_match: score.exact_match,
        match_type: score.match_type.to_string(),
    }
}

fn location_to_wire(loc: fff_query_parser::Location) -> LocationWire {
    match loc {
        fff_query_parser::Location::Line(l) => LocationWire::Line(l),
        fff_query_parser::Location::Position { line, col } => {
            LocationWire::Position { line, col }
        }
        fff_query_parser::Location::Range { start, end } => {
            LocationWire::Range { start, end }
        }
    }
}
