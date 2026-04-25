# FFF Performance Analysis: Why It's Fast

## Overview

FFF achieves sub-millisecond fuzzy search over hundreds of thousands of files by combining several techniques that compound multiplicatively. No single trick accounts for the speed — it's the elimination of overhead at every layer.

---

## 1. Zero IPC (In-Process Execution)

The single largest performance advantage. When used from Neovim, the Rust engine runs **inside the Neovim process** as a dynamically loaded shared library (`.so`/`.dylib`). Function calls cross the LuaJIT C ABI boundary directly — no serialization, no sockets, no pipes, no subprocess spawning.

**Cost of a search call:** One function pointer indirection + argument marshalling to C types. No allocation for the call itself.

Compare to tools that spawn a subprocess per search (e.g., `fzf`, `rg` as external process): each invocation pays process creation (~1ms), pipe setup, serialization, and parsing overhead. FFF pays none of this.

---

## 2. SIMD-Aligned Path Storage

All file paths live in a custom arena of 16-byte aligned chunks (`SimdChunk: #[repr(C, align(16))]`). The fuzzy matcher (`neo_frizbee`) implements Smith-Waterman sequence alignment using NEON (AArch64) or SSE2 (x86_64) SIMD instructions that process 16 bytes per cycle.

**Key insight:** By storing paths pre-aligned to SIMD boundaries, the matcher reads directly from the arena via raw pointers. No string copying, no alignment fixup, no intermediate buffers. Each file's `ChunkedString` resolves to `*const u8` arrays that feed straight into SIMD registers.

**Deduplication bonus:** Common path segments (`src/`, `components/`, `test/`) are stored once and referenced by index. A 100K-file project might have 300K unique path chunks but only 50K distinct chunks after deduplication, reducing both memory and cache pressure.

---

## 3. Smith-Waterman Fuzzy Matching (neo_frizbee)

The fuzzy matching algorithm is Smith-Waterman local alignment — the same algorithm used for biological sequence alignment, known for finding optimal subsequence matches with gap penalties.

**Why not simpler fuzzy matchers?** Simpler approaches (prefix matching, Levenshtein distance) either miss good partial matches or don't handle gaps well. Smith-Waterman finds the optimal alignment between query and candidate, naturally handling typos, abbreviations, and camelCase transitions.

**SIMD implementation:** Each SIMD instruction scores 16 candidate characters against one query character simultaneously. For a 5-character query against a 64-byte path, this is ~20 SIMD instructions instead of ~320 scalar comparisons.

---

## 4. Bigram Prefilter (Inverted Index)

For grep, scanning every file is O(total_bytes). The bigram inverted index reduces this to O(candidate_bytes) where candidates << total.

**How it works:**
1. At index time: extract all byte bigrams from each file's content, set bit `file_i` in the bitset for each bigram
2. At query time: AND the bitsets for all bigrams in the search pattern
3. Result: a bitset of candidate files that could possibly contain the pattern

**Compression:** Prune columns below 3.1% density (too rare = negligible filtering) and above 90% density (too common = useless). The remaining dense columns are ~50% of the original, halving the AND-loop work.

**Incremental overlay:** After initial build, file modifications update a small `AHashMap` overlay. Deletions set tombstone bits. At query time: `(base_candidates & ~tombstones) | overlay_candidates`. No full rebuild needed.

**Impact:** On a 50K-file project, a typical 3-word grep query reduces candidates from 50K to ~200-500 files before any actual string matching begins.

---

## 5. Partial Sort (Top-N Selection)

When the user requests page 1 of results (the common case), FFF doesn't sort all matched files. Instead:

```
if items_needed < total_matched / 2 && total_matched > 100:
    select_nth_unstable_by(items_needed)  // O(n) average
    sort only the top-N                    // O(k log k) where k << n
else:
    glidesort(all)                         // O(n log n) with good cache behavior
```

For 100K matches showing the top 50: O(100K) selection + O(50 * 6) sort ≈ O(100K) total, versus O(100K * 17) for a full sort. This is a ~17x speedup for the first page.

---

## 6. mimalloc + Explicit Memory Return

`mimalloc` is set as the global allocator. Its thread-local heap design eliminates contention for allocation-heavy parallel workloads (scoring, bigram building, file walking).

**Explicit page return:** After building the bigram index (which transiently allocates large `Vec<AtomicU64>` arrays), FFF calls `mi_collect(true)` on every thread in the background pool. This returns freed pages to the OS immediately rather than keeping them in mimalloc's free lists, preventing RSS bloat after one-time setup work.

---

## 7. Lock-Free Content Cache

Each `FileItem` has a `OnceLock<FileContent>` for lazy content caching. `OnceLock` uses atomic operations — after the first initialization, all subsequent reads are a single atomic load (no mutex, no contention).

**Budget control** also uses atomics: `AtomicUsize` for file count, `AtomicU64` for byte count. Budget checks are lock-free compare-and-swap operations that don't serialize concurrent grep workers.

---

## 8. Constraint Short-Circuiting

Before any SIMD matching, constraints are checked:
- `Extension("rs")` → compare last 2 bytes of filename
- `PathSegment("src")` → substring check on resolved path
- `GitStatus(Modified)` → check `FileItem.git_status` field

Files failing constraints are skipped entirely, never reaching the expensive Smith-Waterman matcher. On a query like `*.rs foo`, this eliminates ~80% of files before any fuzzy matching.

---

## 9. Multi-Part Query Narrowing

For queries like `user service handler`:
1. Match all files against "user" → subset A
2. Match only subset A against "service" → subset B
3. Match only subset B against "handler" → subset C

Each part narrows the candidate set geometrically. If part 1 keeps 10% of files and part 2 keeps 10% of those, part 3 only needs to match 1% of the original file count.

---

## 10. Warm-Up Strategy

After the initial scan completes, a background task:
1. Partial-sorts files by `access_frecency_score` (top-N only, O(n))
2. Parallel-maps those files in the rayon thread pool, calling `get_content()` to populate mmap/buffer caches

This ensures that when the user opens the picker for the first time, their most frequently accessed files are already in the OS page cache and the in-process content cache. The first keystroke doesn't pay the cold-cache penalty.

---

## 11. Debounced File Watching

The forked `fff-notify-debouncer-full` accumulates filesystem events for 250ms before processing. This batches rapid file changes (e.g., `git checkout`, build output) into single incremental updates rather than processing each event individually.

The fork also adds `EventKindMask::CORE` filtering and `NoCache` mode to avoid tracking metadata for all watched paths — reducing the watcher's own memory footprint.

---

## 12. Fat LTO + Single Codegen Unit

Release builds use:
```toml
lto = "fat"
codegen-units = 1
```

This enables cross-crate inlining (critical: `fff-grep` matcher → `fff-core` grep loop → `neo_frizbee` SIMD) and allows LLVM to perform whole-program optimization. The single codegen unit ensures no inlining barriers between compilation units.

---

## 13. glidesort

For full sorts, FFF uses `glidesort` instead of the standard library's sort. glidesort is a stable merge-sort variant with better cache behavior on nearly-sorted or partially-ordered data — common in file lists where names cluster by directory.

---

## Performance Bottleneck Summary

| Operation | Dominant cost | Mitigation |
|-----------|--------------|------------|
| First scan | Filesystem walk (I/O bound) | Parallel walker (ignore crate), deferred to background |
| Fuzzy search | SIMD matching over all files | Constraint prefilter, multi-part narrowing, partial sort |
| Grep search | File content reading | Bigram prefilter, content cache, mmap, time budget |
| Result display | Lua table construction | Paginated (only 50 items marshalled per call) |
| Git status | libgit2 full-tree diff | Detached thread during scan, event-driven updates after |
| Scoring | Additive signal computation | Scores pre-cached as i16 on FileItem, lock-free reads |
