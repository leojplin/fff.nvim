# FFF Architecture Report

## Executive Summary

FFF (Fast File Finder) is a high-performance file search engine written in Rust, currently packaged as a Neovim plugin (`fff.nvim`). The engine combines SIMD-accelerated fuzzy matching, a SIMD-aligned path arena, an inverted bigram index for grep prefiltering, LMDB-backed frecency scoring, and a real-time filesystem watcher — all running in-process with zero IPC overhead.

The architecture is already implicitly layered into three tiers: a standalone search core (`fff-core`), a query parser (`fff-query-parser`), and multiple frontend bindings (`fff-nvim`, `fff-c`, `fff-mcp`). This report maps that architecture and identifies the seams for extraction.

---

## Crate Dependency Graph

```
fff-query-parser (zero deps on fff)
       |
       v
fff-grep (zero deps on fff — bstr + memchr only)
       |
       v
   fff-core (package: fff-search)
   /    |    \
  v     v     v
fff-nvim  fff-c  fff-mcp
(cdylib)  (cdylib) (binary)
```

Every upstream crate is a clean library with no Neovim dependency. `fff-core` depends on `fff-query-parser` and `fff-grep`. The three leaf crates are independent frontends.

---

## Layer 1: fff-query-parser

**Zero external dependencies** on fff internals. Pure parser.

| File | Purpose |
|------|---------|
| `parser.rs` | Single-pass tokenizer: splits query into `FuzzyQuery` + `Vec<Constraint>` + optional `Location` |
| `constraints.rs` | Enum: `Extension`, `Glob`, `PathSegment`, `Not(Box<Constraint>)`, `GitStatus`, `FileType`, `FilePath`, `Text` |
| `config.rs` | Trait `ParserConfig` with implementations for file search, grep, AI grep, dir search, mixed search |
| `glob_detect.rs` | Glob pattern detection utilities |
| `location.rs` | `Location` struct (line, col) parsed from `:line:col` suffixes |

Constraint syntax (prefix-based): `*.rs` (extension), `/path/` (path segment), `!term` (negation), `@modified` (git status).

---

## Layer 2: fff-grep

**Minimal grep engine.** Only depends on `bstr` and `memchr`.

Reimplements `grep-searcher` from the ripgrep family but stripped to a single code path: `search_slice`. No file I/O, no mmap, no reader abstraction. This eliminates the heavy transitive dependency tree of `grep-searcher` while providing the exact API surface fff-core needs.

Key types:
- `Matcher` trait with `find_at(haystack, offset) -> Option<Match>`
- `Searcher` with `search_slice<M, S>()` dispatching to line-by-line or multi-line mode
- `Sink` trait (push model) with `matched()`, `begin()`, `finish()` callbacks

---

## Layer 3: fff-core (package: fff-search)

The engine. 7 major subsystems:

### 3.1 SIMD Path Arena (`simd_path.rs`)

All file paths are stored in a custom arena of 16-byte SIMD-aligned chunks (`SimdChunk: #[repr(C, align(16))]`). Paths are represented as `ChunkedString` — a `SmallVec<[u32; 4]>` of chunk indices + metadata.

**Why:** The fuzzy matcher (`neo_frizbee`) uses SIMD Smith-Waterman alignment that processes 16 bytes per instruction. Storing paths as pre-aligned 16-byte chunks means the matcher can operate directly on arena memory via raw pointers — zero string copies at search time.

**Deduplication:** `ChunkedPathStoreBuilder` uses `AHashMap<[u8;16], u32>` to deduplicate chunks. Common directory prefixes like `src/` occupy a single chunk shared by thousands of files.

### 3.2 File Picker (`file_picker.rs`, 2417 lines)

The orchestrator. `FilePicker` owns:
- `FileSync`: `Vec<FileItem>` + `Vec<DirItem>` + path arenas + bigram index
- Background scan via `ignore::WalkBuilder::build_parallel()` (same walker as ripgrep)
- `BackgroundWatcher` for filesystem events
- Content cache budget control

**Scan pipeline:**
1. Parallel walk (ignore crate, `num_cpus - 2` threads)
2. Parallel sort by relative path (rayon)
3. Single pass: build `ChunkedPathStore` + `Vec<DirItem>` simultaneously
4. Re-sort files by `(parent_dir, filename_offset)` for binary search
5. Concurrent: git status collection via libgit2 on a detached thread
6. Post-scan: bigram index build + frecent file mmap warmup

**Overflow handling:** Files created after the initial scan go into a secondary `ChunkedPathStoreBuilder` (overflow arena). The overflow section supports linear scan; the base section supports binary search.

### 3.3 Scoring Engine (`score.rs`, 1465 lines)

Multi-signal scoring with additive components:

| Signal | Source | Weight |
|--------|--------|--------|
| Base score | SIMD Smith-Waterman alignment (frizbee) | 0–100 (normalized) |
| Filename bonus | Match in filename vs directory portion | +40% of base |
| Special filename bonus | `index.ts`, `mod.rs`, `__init__.py`, etc. | +5% |
| Path alignment bonus | Query looks path-like, suffix matches | Variable |
| Frecency boost | Exponential decay (10-day half-life for access) | Additive |
| Git status boost | Modified/untracked/staged files | Additive |
| Distance penalty | Path depth from project root | Subtractive |
| Current file penalty | Suppress self-match | Subtractive |
| Combo boost | Historical query→file associations | +1000 or scaled |

**Sort optimization:** When `items_needed < total/2` and `total > 100`, uses `select_nth_unstable_by` (O(n) partial sort) instead of full sort. Only the top-N candidates are then fully sorted.

### 3.4 Bigram Inverted Index (`bigram_filter.rs`, 528 lines)

For grep prefiltering. A bitset-based inverted index over all 65,536 possible byte bigrams.

**Build:** After initial scan, each file's content is scanned for bigrams. Each bigram maps to a bitset where bit `i` = "file `i` contains this bigram."

**Compression:** Columns with <3.1% density (rare bigrams) or >90% density (ubiquitous bigrams like `e `) are pruned. The remaining "dense" columns are packed into contiguous `Vec<u64>`.

**Query:** AND all bigram bitsets for the search pattern. The result is a candidate set — only these files need actual grep scanning.

**Incremental updates:** `BigramOverlay` is a delta layer — `AHashMap<file_idx, Vec<bigram>>` for modifications + `Vec<u64>` tombstone bitset for deletions. Merged at query time: `(base & ~tombstones) | overlay`.

### 3.5 Grep Engine (`grep.rs`, 2619 lines)

Three matcher backends:
- `PlainTextMatcher`: case-insensitive `memchr`-based literal search
- `RegexMatcher`: `regex` crate with smart case
- `AhoCorasickMatcher`: multi-pattern via Aho-Corasick automaton

**Execution:** Rayon parallel iteration over candidate files (pre-filtered by bigram index). Each file's content is mmap'd or read into a buffer, then searched via the selected matcher. Results are collected with budget controls (`time_budget_ms`, `max_matches_per_file`).

### 3.6 Frecency Tracker (`frecency.rs`, 577 lines)

LMDB database mapping `blake3::hash(path)` → `VecDeque<u64>` (timestamps).

- Access frecency: `sum(exp(-0.0693 * days_ago))` per timestamp, 10-day half-life
- Modification frecency: piecewise linear over thresholds (16 pts at 2 min → 1 pt at 1 week)
- Scores stored as `i16` on `FileItem` for lock-free read during scoring
- Background GC thread purges entries older than 30 days + LMDB compaction

### 3.7 Background Watcher (`background_watcher.rs`, 724 lines)

Uses `notify` (forked `fff-notify-debouncer-full`) with:
- 250ms debounce window
- Git event detection: watches `.git/` and `.git/info/` for index/HEAD changes
- Events trigger incremental updates to `FileSync`: file create → append to overflow, modify → update bigram overlay, delete → tombstone

---

## Layer 4: Frontend Bindings

### 4.1 fff-nvim (Neovim Lua Module)

`cdylib` loaded via `package.loadlib` into Neovim's LuaJIT runtime. **Zero IPC** — Rust functions are called directly across the C ABI boundary.

Global state: `Lazy<SharedPicker>`, `Lazy<SharedFrecency>`, `Lazy<SharedQueryTracker>` — one instance per Neovim process.

25+ Lua-callable functions exported via `mlua::lua_module`. The Lua layer (`lua/fff/*.lua`) provides:
- Picker UI (floating windows, keymaps, preview, scrollbar)
- Configuration management
- Treesitter integration for grep result highlighting
- Git status highlight groups

### 4.2 fff-c (C FFI Library)

`cdylib` with `cbindgen`-generated header. All structs are `#[repr(C)]`. Named accessor functions for every field (stable ABI for consumers like Emacs Lisp).

`FffInstance` opaque handle pattern: `init → search → free`.

### 4.3 fff-mcp (MCP Server Binary)

Async binary (tokio + rmcp) exposing `find_files`, `grep`, `multi_grep` tools over stdio transport. Designed for AI code assistants. Includes auto-broadening search strategies and smart output truncation.

---

## Threading Model

| Thread/Pool | Lifetime | Purpose |
|-------------|----------|---------|
| `BACKGROUND_THREAD_POOL` (rayon, N = cpus-2) | Process lifetime | Parallel scoring, bigram build, mmap warmup |
| Scan thread | One per `FilePicker` init | `walk_filesystem()` + post-scan setup |
| Git status thread | Detached during scan | libgit2 status collection |
| Debouncer thread | Per `BackgroundWatcher` | 250ms event batching |
| Frecency GC thread | One-shot after init | LMDB purge + compaction |
| Query tracker threads | Per query completion | Async LMDB writes |

---

## Memory Model

- **Global allocator:** `mimalloc` (thread-local heaps, low fragmentation)
- **Path arena:** SIMD-aligned, deduplicated, immutable after scan
- **Content cache:** Lazy `OnceLock<FileContent>` per file, budget-controlled (512 MB default)
- **mmap threshold:** >=16 KB (AArch64) / >=4 KB (x86_64) uses `memmap2::Mmap`; smaller files use `Vec<u8>`
- **Post-build collection:** `mi_collect(true)` on all pool threads after bigram build to return pages to OS
- **FileItem target:** 64 bytes per file entry

---

## Existing Separation Seams

The codebase already has clean boundaries suitable for extraction:

1. **`fff-core` is already a standalone library** — no Neovim, no Lua, no UI. It has a public API with doc examples showing standalone usage.
2. **`fff-query-parser` is already zero-dependency** on the rest of fff.
3. **`fff-grep` is already standalone** — only `bstr` + `memchr`.
4. **`fff-c` already wraps fff-core for non-Neovim consumers.**
5. **`fff-mcp` already runs as a standalone binary** using fff-core directly.

The primary coupling is that `fff-nvim` uses global `Lazy` statics for `SharedPicker/Frecency/QueryTracker`, which are already `Arc<RwLock<Option<T>>>` — the exact pattern needed for a daemon that serves multiple clients.
