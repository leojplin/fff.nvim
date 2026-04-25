# FFF Domain Mapping

## Functional Components

This document maps every functional domain in the FFF codebase to its implementing files, public API surface, and data flow relationships.

---

## Domain 1: Query Parsing

**Crate:** `fff-query-parser`

**Responsibility:** Parse user query strings into structured search parameters.

| File | Key Types/Functions |
|------|-------------------|
| `parser.rs` | `QueryParser`, `FFFQuery`, `FuzzyQuery`, `TextPartsBuffer` |
| `constraints.rs` | `Constraint` enum (8 variants), `GitStatusFilter` |
| `config.rs` | `ParserConfig` trait, `FileSearchConfig`, `GrepConfig`, `AiGrepConfig`, `DirSearchConfig`, `MixedSearchConfig` |
| `location.rs` | `Location { line, col }` |
| `glob_detect.rs` | Glob pattern detection |

**Input:** Raw query string (e.g., `"*.rs user service :42"`)
**Output:** `FFFQuery { fuzzy_query, constraints, location }`

**Data flow:**
```
User input → QueryParser::parse(query) → FFFQuery
                                            ├─ FuzzyQuery::Parts(["user", "service"])
                                            ├─ Constraint::Extension("rs")
                                            └─ Location { line: 42 }
```

---

## Domain 2: Filesystem Indexing

**Crate:** `fff-core`

**Responsibility:** Walk a directory tree, build an in-memory index of all files, maintain it incrementally.

| File | Key Types/Functions |
|------|-------------------|
| `file_picker.rs` | `FilePicker`, `FileSync`, `WalkResult`, `FilePickerOptions`, `FFFMode` |
| `simd_path.rs` | `ChunkedPathStore`, `ChunkedPathStoreBuilder`, `ChunkedString`, `SimdChunk`, `ArenaPtr` |
| `types.rs` | `FileItem`, `DirItem`, `FileContent`, `ContentCacheBudget` |
| `background_watcher.rs` | `BackgroundWatcher`, debounce config, git event detection |
| `ignore.rs` | Custom ignore rule handling |

**Data flow:**
```
base_path → ignore::WalkBuilder::build_parallel()
         → Mutex<Vec<(FileItem, rel_path)>>
         → parallel_sort (rayon)
         → ChunkedPathStoreBuilder::finish() → ChunkedPathStore
         → Vec<DirItem> (built in same pass)
         → files re-sorted by (parent_dir, filename_offset)
         → FileSync { files, dirs, chunked_paths, bigram_index }
         → BackgroundWatcher (incremental updates)
```

**Overflow path (runtime additions):**
```
notify event (create) → overflow_builder.add_path()
                      → files.push(new FileItem)
                      → bigram_overlay.modify_file()
```

---

## Domain 3: Fuzzy Search

**Crate:** `fff-core`

**Responsibility:** Match a parsed query against indexed files, score results, return paginated results.

| File | Key Types/Functions |
|------|-------------------|
| `score.rs` | `fuzzy_match_and_score_files`, `fuzzy_match_and_score_dirs`, `match_and_score_in_arena`, `match_fuzzy_parts`, `sort_and_paginate` |
| `types.rs` | `Score`, `ScoringContext`, `SearchResult`, `DirSearchResult`, `MixedSearchResult`, `Pagination` |
| `file_picker.rs` | `FilePicker::fuzzy_search()`, `FilePicker::dir_search()`, `FilePicker::mixed_search()` |

**Data flow:**
```
FFFQuery + ScoringContext
  → constraint prefilter (skip non-matching files)
  → neo_frizbee SIMD matching (Smith-Waterman alignment)
  → Score accumulation (base + filename + frecency + git + combo + distance)
  → sort_and_paginate (partial sort or glidesort)
  → SearchResult { items, total_matched, scores }
```

---

## Domain 4: Grep Search

**Crate:** `fff-core` + `fff-grep`

**Responsibility:** Search file contents for patterns using plain text, regex, or fuzzy matching.

| File | Key Types/Functions |
|------|-------------------|
| `grep.rs` | `perform_grep`, `grep_search`, `fuzzy_grep_search`, `multi_grep_search`, `GrepMode`, `GrepMatch`, `GrepSearchOptions` |
| `grep.rs` | `PlainTextMatcher`, `RegexMatcher`, `AhoCorasickMatcher` |
| `bigram_filter.rs` | `BigramFilter`, `BigramIndexBuilder`, `BigramOverlay` |
| `fff-grep/lib.rs` | `Searcher`, `Matcher` trait, `Sink` trait, line iteration |

**Data flow:**
```
query + GrepMode
  → extract bigrams from pattern
  → BigramFilter::query() → candidate bitset
  → merge with BigramOverlay (tombstones + modifications)
  → rayon parallel iter over candidate files
  → per-file: get_content() → Matcher::find_at() → Sink::matched()
  → collect GrepMatch results
  → sort by frecency + score
  → GrepResult { items, total_matched, next_file_offset }
```

---

## Domain 5: Frecency Tracking

**Crate:** `fff-core`

**Responsibility:** Persist file access patterns and compute recency-weighted popularity scores.

| File | Key Types/Functions |
|------|-------------------|
| `frecency.rs` | `FrecencyTracker`, `compute_access_frecency`, `compute_modification_frecency`, `purge_stale_entries` |
| `types.rs` | `FileItem::access_frecency_score`, `FileItem::modification_frecency_score` (both `i16`) |

**Storage:** LMDB via `heed`. Key = `blake3::hash(path)` (32 bytes). Value = `VecDeque<u64>` (ring buffer of Unix timestamps).

**Data flow:**
```
BufEnter (Lua autocmd) → fuzzy.track_access(path)
  → FrecencyTracker::record_access(path, timestamp)
  → LMDB write
  → FileItem.access_frecency_score updated (brief write lock on FilePicker)

Score computation:
  timestamps → sum(exp(-0.0693 * days_ago)) → normalized to i16
```

---

## Domain 6: Query History / Combo Boost

**Crate:** `fff-core`

**Responsibility:** Track which files users select for each query string, boost repeated query→file pairs.

| File | Key Types/Functions |
|------|-------------------|
| `query_tracker.rs` | `QueryTracker`, `track_query_completion`, `get_historical_queries` |
| `score.rs` | Combo boost logic in scoring pipeline |

**Storage:** LMDB. Maps `query_string → Vec<(file_path, open_count)>`.

**Data flow:**
```
User selects file for query → QueryTracker::track_completion(query, file_path)
                            → LMDB: increment open_count for (query, file)

Next search with same query → QueryTracker::get_combo_data(query)
                             → Score::combo_match_boost = open_count * multiplier
```

---

## Domain 7: Git Integration

**Crate:** `fff-core`

**Responsibility:** Detect git repositories, collect file statuses, watch for git events.

| File | Key Types/Functions |
|------|-------------------|
| `git.rs` (in file_picker context) | `git2::Repository::discover()`, `git2::StatusOptions`, git status collection |
| `background_watcher.rs` | Git event detection (`.git/index`, `.git/HEAD` changes) |
| `types.rs` | `FileItem::git_status: Option<git2::Status>` |
| `score.rs` | `git_status_boost` scoring signal |

**Data flow:**
```
Initial scan → detached thread: git2::Repository::statuses()
             → per-file: FileItem.git_status = Some(status)

Runtime → BackgroundWatcher detects .git/ changes
        → refresh_git_status() → re-collect statuses
        → update FileItem.git_status in-place
```

---

## Domain 8: Shared State Management

**Crate:** `fff-core`

**Responsibility:** Thread-safe access to engine singletons.

| File | Key Types/Functions |
|------|-------------------|
| `shared.rs` | `SharedPicker(Arc<parking_lot::RwLock<Option<FilePicker>>>)` |
| `shared.rs` | `SharedFrecency(Arc<std::sync::RwLock<Option<FrecencyTracker>>>)` |
| `shared.rs` | `SharedQueryTracker(Arc<std::sync::RwLock<Option<QueryTracker>>>)` |

`parking_lot::RwLock` is used for `SharedPicker` (reader-fair, no writer starvation under concurrent search requests). Standard `RwLock` is used for the less-contended frecency and query tracker.

---

## Domain 9: Neovim UI

**Crate:** `fff-nvim` (Rust) + `lua/fff/*.lua` (Lua)

**Responsibility:** Picker window management, input handling, result rendering, preview, keybindings.

| File | Key Types/Functions |
|------|-------------------|
| `picker_ui.lua` | `create_ui`, `on_input_change`, `update_results_sync`, `render_list`, `select`, `close` |
| `list_renderer.lua` | `ListRenderContext`, buffer writes, highlight application |
| `file_renderer.lua` | `render_line`, `apply_highlights` for file items |
| `grep/grep_renderer.lua` | Grouped grep results with treesitter highlighting |
| `combo_renderer.lua` | "Last Match (xN combo)" overlay windows |
| `scrollbar.lua` | Pagination scrollbar widget |
| `file_picker/preview.lua` | Streaming async file preview with chunked loading |
| `treesitter_hl.lua` | Inline syntax highlighting via scratch buffer cache |

**Data flow:**
```
Keystroke → nvim_buf_attach on_lines → on_input_change()
  → file_picker.search_files_paginated() or grep.search()
    → Rust: fuzzy_search() or live_grep()
  → items stored in M.state
  → render_debounced() → vim.schedule → render_list()
  → list_renderer writes to buffer + applies highlights
  → update_preview_smart() → preview.lua async file load
```

---

## Domain 10: Configuration

**Crate:** Lua only (`lua/fff/conf.lua`)

**Responsibility:** Merge user config with defaults, handle deprecation migration, support dynamic values.

| File | Key Types/Functions |
|------|-------------------|
| `conf.lua` | `M.get()`, `default_config`, `handle_deprecated_config`, deprecation rules |
| `utils.lua` | `resolve_config_value(value, w, h, validator, fallback)` — supports function-valued config |

Config values for layout dimensions can be functions: `function(terminal_width, terminal_height) → value`. This enables responsive layouts without configuration conditionals.

---

## Domain 11: Binary Distribution

**Crate:** Lua (`lua/fff/download.lua`, `lua/fff/utils/version.lua`, `lua/fff/utils/system.lua`)

**Responsibility:** Download pre-built binaries or fall back to cargo build.

| File | Key Types/Functions |
|------|-------------------|
| `download.lua` | `download_from_github`, `ensure_downloaded`, `download_or_build_binary` |
| `utils/version.lua` | `current_release_tag`, `resolve` (nightly version from git) |
| `utils/system.lua` | `get_triple` (target triple), `get_lib_extension` |

---

## Cross-Domain Dependencies

```
Query Parsing ──────────────────────────┐
                                        v
Filesystem Indexing ──→ Bigram Index ──→ Grep Search
        |                                    |
        v                                    v
   SIMD Path Arena ──→ Fuzzy Search ──→ Scoring Engine
                            ^                ^
                            |                |
                    Frecency Tracking   Combo Boost
                            ^                ^
                            |                |
                    Git Integration    Query History
                            |
                            v
                    Shared State ──→ Frontend Bindings
                                    ├─ Neovim UI
                                    ├─ C FFI
                                    └─ MCP Server
```

---

## Neovim-Specific vs Engine-Generic

| Domain | Neovim-specific? | Notes |
|--------|-----------------|-------|
| Query Parsing | No | Zero fff deps |
| Filesystem Indexing | No | Pure Rust |
| Fuzzy Search | No | Pure Rust |
| Grep Search | No | Pure Rust |
| Frecency Tracking | No | LMDB, no UI |
| Query History | No | LMDB, no UI |
| Git Integration | No | libgit2, no UI |
| Bigram Index | No | Pure Rust |
| Shared State | No | Generic Arc/RwLock |
| Neovim UI | **Yes** | Lua + Neovim APIs |
| Configuration | **Partially** | Defaults reference Neovim paths |
| Binary Distribution | **Yes** | Lua, assumes lazy.nvim |
