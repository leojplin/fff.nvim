use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::constraints::Constrainable;
use crate::query_tracker::QueryMatchEntry;
use crate::simd_path::PATH_BUF_SIZE;
use fff_query_parser::{FFFQuery, FuzzyQuery, Location};

/// Cached file contents — mmap on Unix, heap buffer on Windows.
///
/// On Windows, memory-mapped files hold the file handle open and prevent
/// editors from saving (writing/replacing) those files. Reading into a
/// `Vec<u8>` releases the handle immediately after the read completes.
///
/// The `Buffer` variant is also used on Unix for temporary (uncached) reads
/// where the mmap/munmap syscall overhead exceeds the cost of a heap copy.
#[derive(Debug)]
#[allow(dead_code)] // variants are conditionally used per platform
pub enum FileContent {
    #[cfg(not(target_os = "windows"))]
    Mmap(memmap2::Mmap),
    Buffer(Vec<u8>),
}

impl std::ops::Deref for FileContent {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(not(target_os = "windows"))]
            FileContent::Mmap(m) => m,
            FileContent::Buffer(b) => b,
        }
    }
}

pub struct FileItemFlags;

impl FileItemFlags {
    pub const BINARY: u8 = 1 << 0;
    /// Tombstone — file was deleted but index slot is preserved so
    /// bigram indices for other files stay valid.
    pub const DELETED: u8 = 1 << 1;
}

/// A directory entry with aggregated metadata from its child files.
/// Stored in a sorted `Vec<DirItem>` (the "dir table") inside `FileSync`,
/// giving O(log n) lookup by path and enabling directory picker mode.
#[derive(Debug, Clone)]
pub struct DirItem {
    /// Absolute path of the directory (with trailing separator removed).
    path: String,
    /// Byte offset where the relative path begins.
    relative_start: u16,
}

impl DirItem {
    pub fn new(path: String, relative_start: u16) -> Self {
        Self {
            path,
            relative_start,
        }
    }

    /// The full absolute path as a string slice.
    #[inline]
    pub fn path_str(&self) -> &str {
        &self.path
    }

    /// The full absolute path as a `&Path`.
    #[inline]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.path)
    }

    /// The relative path from the base directory (without trailing separator).
    /// For the base directory itself, returns "".
    #[inline]
    pub fn relative_path(&self) -> &str {
        &self.path[self.relative_start as usize..]
    }
}

impl neo_frizbee::Matchable for DirItem {
    #[inline]
    fn match_str(&self) -> Option<&str> {
        let rel = self.relative_path();
        if rel.is_empty() { None } else { Some(rel) }
    }
}

/// A single indexed file with metadata, frecency scores, and lazy content cache.
///
/// File contents are initialized lazily on the first grep access and cached for
/// subsequent searches. On Unix, uses mmap backed by the kernel page cache. On
/// Windows, reads into a heap buffer to avoid holding file handles open.
///
/// Thread-safety: `OnceLock` provides lock-free reads after initialization.
/// Each file is only searched by one rayon worker at a time via `par_iter`.
///
/// Path storage uses a `ChunkedString` backed by the shared SIMD chunk arena.
/// The `ChunkedString` stores indices into deduplicated 16-byte chunks and
/// knows the filename offset, enabling zero-copy SIMD matching and efficient
/// dir/filename extraction.
#[derive(Debug)]
pub struct FileItem {
    /// File size in bytes
    pub size: u64,
    /// Modification time in UNIX timestamp
    pub modified: u64,
    /// Frecency access score
    pub access_frecency_score: i16,
    /// Frecency modification score
    pub modification_frecency_score: i16,
    /// The file's git status
    pub git_status: Option<git2::Status>,

    /// Relative path stored as indices into the shared SIMD chunk arena.
    /// Knows the filename offset for efficient dir/filename extraction.
    /// Initialized as `empty()` during walk phase, then populated by
    /// `build_chunked_path_store_from_strings` or set directly via `set_path`.
    pub path: crate::simd_path::ChunkedString,
    /// Index into the dir table (`FileSync::dirs`).
    parent_dir: u32,
    /// Packed boolean flags — see `FileItemFlags`.
    flags: u8,
    /// Lazily-initialized file contents for grep.
    content: OnceLock<FileContent>,
}

impl Clone for FileItem {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            parent_dir: self.parent_dir,
            size: self.size,
            modified: self.modified,
            access_frecency_score: self.access_frecency_score,
            modification_frecency_score: self.modification_frecency_score,
            git_status: self.git_status,
            flags: self.flags,
            content: OnceLock::new(),
        }
    }
}

impl FileItem {
    /// Create a new `FileItem` with an empty `ChunkedString` placeholder.
    ///
    /// The `filename_start` is stored in `path.filename_offset` so the
    /// arena builder knows the dir/filename split point. The path data
    /// itself is NOT functional until `set_path` populates it.
    ///
    /// For test convenience, callers that don't use the arena builder can
    /// construct a `ChunkedString` via `build_chunked_path_store_from_strings`
    /// and then assign it with `set_path`.
    pub fn new_raw(
        filename_start: u16,
        size: u64,
        modified: u64,
        git_status: Option<git2::Status>,
        is_binary: bool,
    ) -> Self {
        let mut flags = 0u8;
        if is_binary {
            flags |= FileItemFlags::BINARY;
        }

        let mut path = crate::simd_path::ChunkedString::empty();
        path.filename_offset = filename_start;

        Self {
            path,
            parent_dir: u32::MAX,
            size,
            modified,
            access_frecency_score: 0,
            modification_frecency_score: 0,
            git_status,
            flags,
            content: OnceLock::new(),
        }
    }

    /// Replace this item's path with a fully-initialized `ChunkedString`.
    #[inline]
    pub fn set_path(&mut self, path: crate::simd_path::ChunkedString) {
        self.path = path;
    }

    /// Index into the dir table for this file's parent directory.
    #[inline]
    pub fn parent_dir_index(&self) -> u32 {
        self.parent_dir
    }

    /// Set the parent directory index.
    #[inline]
    pub fn set_parent_dir(&mut self, idx: u32) {
        self.parent_dir = idx;
    }

    /// The directory portion of the relative path. **Allocates** (cold path).
    ///
    /// `arena` is `ChunkedPathStore::arena_base_ptr()`.
    #[inline]
    pub fn dir_str(&self, arena: *const u8) -> String {
        self.path.dir_string(arena)
    }

    /// The filename component. **Allocates** (cold path).
    #[inline]
    pub fn file_name(&self, arena: *const u8) -> String {
        self.path.file_name_string(arena)
    }

    /// The full relative path. **Allocates** (cold path).
    #[inline]
    pub fn relative_path(&self, arena: *const u8) -> String {
        self.path.to_string(arena)
    }

    /// Write the full relative path into a caller buffer (hot path, zero-alloc).
    #[inline]
    pub fn write_relative_path<'a>(&self, arena: *const u8, buf: &'a mut [u8]) -> &'a str {
        self.path.read_to_buf(arena, buf)
    }

    /// Total byte length of the relative path.
    #[inline]
    pub fn relative_path_len(&self) -> usize {
        self.path.byte_len as usize
    }

    /// Byte offset of the filename within the relative path.
    #[inline]
    pub fn filename_offset_in_relative(&self) -> usize {
        self.path.filename_offset as usize
    }

    /// Check if the relative path equals `other` without heap allocation.
    #[inline]
    pub fn relative_path_eq(&self, arena: *const u8, other: &str) -> bool {
        if other.len() != self.path.byte_len as usize {
            return false;
        }
        let mut buf = [0u8; PATH_BUF_SIZE];
        let mine = self.path.read_to_buf(arena, &mut buf);
        mine == other
    }

    /// Check if the relative path ends with `suffix` without heap allocation.
    #[inline]
    pub fn relative_path_ends_with(&self, arena: *const u8, suffix: &str) -> bool {
        let mut buf = [0u8; PATH_BUF_SIZE];
        let path = self.path.read_to_buf(arena, &mut buf);
        if suffix.len() > path.len() {
            return false;
        }
        path.ends_with(suffix)
    }

    /// Check if the relative path starts with `prefix` without heap allocation.
    #[inline]
    pub fn relative_path_starts_with(&self, arena: *const u8, prefix: &str) -> bool {
        let mut buf = [0u8; PATH_BUF_SIZE];
        let path = self.path.read_to_buf(arena, &mut buf);
        path.starts_with(prefix)
    }

    /// Reconstruct the full absolute path. Cold-path only (allocates).
    #[inline]
    pub fn absolute_path(&self, arena: *const u8, base_path: &Path) -> PathBuf {
        let mut buf = [0u8; PATH_BUF_SIZE];
        let rel = self.path.read_to_buf(arena, &mut buf);
        base_path.join(rel)
    }

    /// Write the full absolute path into a caller-provided buffer (zero-alloc).
    /// Returns `&Path` over the written bytes.
    #[inline]
    pub fn write_absolute_path<'a>(
        &self,
        arena: *const u8,
        base_path: &Path,
        buf: &'a mut [u8; PATH_BUF_SIZE],
    ) -> &'a Path {
        let base = base_path.as_os_str().as_encoded_bytes();
        let base_len = base.len();
        buf[..base_len].copy_from_slice(base);
        // Add separator if base doesn't end with one
        let sep_len = if base_len > 0 && base[base_len - 1] != b'/' {
            buf[base_len] = b'/';
            1
        } else {
            0
        };
        let rel_start = base_len + sep_len;
        let mut rel_buf = [0u8; PATH_BUF_SIZE];
        let rel = self.path.read_to_buf(arena, &mut rel_buf);
        let rel_bytes = rel.as_bytes();
        buf[rel_start..rel_start + rel_bytes.len()].copy_from_slice(rel_bytes);
        let total = rel_start + rel_bytes.len();
        Path::new(unsafe { std::str::from_utf8_unchecked(&buf[..total]) })
    }

    #[inline]
    pub fn total_frecency_score(&self) -> i32 {
        self.access_frecency_score as i32 + self.modification_frecency_score as i32
    }

    #[inline]
    pub fn is_binary(&self) -> bool {
        self.flags & FileItemFlags::BINARY != 0
    }

    #[inline]
    pub fn set_binary(&mut self, val: bool) {
        if val {
            self.flags |= FileItemFlags::BINARY;
        } else {
            self.flags &= !FileItemFlags::BINARY;
        }
    }

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & FileItemFlags::DELETED != 0
    }

    #[inline]
    pub fn set_deleted(&mut self, val: bool) {
        if val {
            self.flags |= FileItemFlags::DELETED;
        } else {
            self.flags &= !FileItemFlags::DELETED;
        }
    }
}

impl FileItem {
    /// Invalidate the cached content so the next `get_content()` call creates a fresh one.
    ///
    /// Call this when the background watcher detects that the file has been modified.
    /// On Unix, a file that is truncated while mapped can cause SIGBUS. On Windows,
    /// the stale buffer simply won't reflect the new contents. In both cases,
    /// invalidating ensures a fresh read on the next access.
    pub fn invalidate_mmap(&mut self, budget: &ContentCacheBudget) {
        if self.content.get().is_some() {
            budget.cached_count.fetch_sub(1, Ordering::Relaxed);
            budget.cached_bytes.fetch_sub(self.size, Ordering::Relaxed);
        }

        self.content = OnceLock::new();
    }

    /// Get the cached file contents or lazily load and cache them.
    ///
    /// Returns `None` if the file is too large, empty, can't be opened, **or
    /// the cache budget is exhausted**. Callers that need content regardless
    /// of the budget should use [`get_content_for_search`].
    ///
    /// After the first call, this is lock-free (just an atomic load + pointer deref).
    pub fn get_content(
        &self,
        arena: *const u8,
        base_path: &Path,
        budget: &ContentCacheBudget,
    ) -> Option<&[u8]> {
        if let Some(content) = self.content.get() {
            return Some(content);
        }

        let max_file_size = budget.max_file_size;
        if self.size == 0 || self.size > max_file_size {
            return None;
        }

        // Check cache budget before creating a new persistent cache entry.
        let count = budget.cached_count.load(Ordering::Relaxed);
        let bytes = budget.cached_bytes.load(Ordering::Relaxed);
        let max_files = budget.max_files;
        let max_bytes = budget.max_bytes;
        if count >= max_files || bytes + self.size > max_bytes {
            return None;
        }

        let content = load_file_content(&self.absolute_path(arena, base_path), self.size)?;
        let result = self.content.get_or_init(|| content);

        // Bump counters. Slight over-count under races is fine — the budget
        // is a soft limit and the overshoot is bounded by rayon thread count.
        budget.cached_count.fetch_add(1, Ordering::Relaxed);
        budget.cached_bytes.fetch_add(self.size, Ordering::Relaxed);

        Some(result)
    }

    /// Get file content for searching — **always returns content** for eligible
    /// files, even when the persistent cache budget is exhausted.
    #[inline]
    pub fn get_content_for_search<'a>(
        &'a self,
        buf: &'a mut Vec<u8>,
        budget: &ContentCacheBudget,
    ) -> Option<&'a [u8]> {
        // Fast path: persistent cache hit (zero-copy).
        if let Some(cached) = self.get_content(budget) {
            return Some(cached);
        }

        let max_file_size = budget.max_file_size;
        if self.is_binary() || self.size == 0 || self.size > max_file_size {
            return None;
        }

        // Slow path: read into the reusable buffer — open() + read_exact() + close().
        // No mmap()/munmap() syscalls, no page table setup/teardown.
        // We know the exact size so we use read_exact (1 read syscall) instead of
        // read_to_end (2 read syscalls — one for data, one for EOF confirmation).
        let len = self.size as usize;
        buf.resize(len, 0);
        let mut file = std::fs::File::open(self.as_path()).ok()?;
        file.read_exact(buf).ok()?;
        Some(buf.as_slice())
    }
}

/// Page size on Apple Silicon is 16KB; on x86-64 it's 4KB.
/// Files smaller than one page waste the remainder when mmapped.
/// Reading them into a heap buffer avoids this overhead.
#[cfg(target_arch = "aarch64")]
const MMAP_THRESHOLD: u64 = 16 * 1024;
#[cfg(not(target_arch = "aarch64"))]
const MMAP_THRESHOLD: u64 = 4 * 1024;

/// Load file contents: small files are read into a heap buffer to avoid
/// mmap page alignment waste; large files use mmap for zero-copy access.
/// On Windows, always uses heap buffer (mmap holds the file handle open).
fn load_file_content(path: &Path, size: u64) -> Option<FileContent> {
    #[cfg(not(target_os = "windows"))]
    {
        if size < MMAP_THRESHOLD {
            let data = std::fs::read(path).ok()?;
            Some(FileContent::Buffer(data))
        } else {
            let file = std::fs::File::open(path).ok()?;
            // SAFETY: The mmap is backed by the kernel page cache and automatically
            // reflects file modifications. The only risk is SIGBUS if the file is
            // truncated while mapped.
            let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
            Some(FileContent::Mmap(mmap))
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = size;
        let data = std::fs::read(path).ok()?;
        Some(FileContent::Buffer(data))
    }
}

impl Constrainable for FileItem {
    #[inline]
    fn write_file_name<'a>(&self, arena: *const u8, buf: &'a mut [u8]) -> &'a str {
        self.path.file_name(arena, buf)
    }

    #[inline]
    fn write_relative_path<'a>(&self, arena: *const u8, buf: &'a mut [u8]) -> &'a str {
        self.path.read_to_buf(arena, buf)
    }

    #[inline]
    fn git_status(&self) -> Option<git2::Status> {
        self.git_status
    }
}

#[derive(Debug, Clone, Default)]
pub struct Score {
    pub total: i32,
    pub base_score: i32,
    pub filename_bonus: i32,
    pub special_filename_bonus: i32,
    pub frecency_boost: i32,
    pub git_status_boost: i32,
    pub distance_penalty: i32,
    pub current_file_penalty: i32,
    pub combo_match_boost: i32,
    pub path_alignment_bonus: i32,
    pub exact_match: bool,
    pub match_type: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct PaginationArgs {
    pub offset: usize,
    pub limit: usize,
}

impl Default for PaginationArgs {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
        }
    }
}

/// Context for scoring files during search.
///
/// The `query` field contains the pre-parsed query with constraints,
/// fuzzy parts, and location information. Parsing is done once at the API
/// boundary and passed through.
#[derive(Debug, Clone)]
pub struct ScoringContext<'a> {
    /// Parsed query containing raw text, constraints, fuzzy parts, and location
    pub query: &'a FFFQuery<'a>,
    pub project_path: Option<&'a Path>,
    pub current_file: Option<&'a str>,
    pub max_typos: u16,
    pub max_threads: usize,
    pub last_same_query_match: Option<QueryMatchEntry>,
    pub combo_boost_score_multiplier: i32,
    pub min_combo_count: u32,
    pub pagination: PaginationArgs,
}

impl ScoringContext<'_> {
    /// Get the effective fuzzy query string for matching.
    /// Returns the first fuzzy part, or the raw query if no parsing was done.
    pub fn effective_query(&self) -> &str {
        match &self.query.fuzzy_query {
            FuzzyQuery::Text(t) => t,
            FuzzyQuery::Parts(parts) if !parts.is_empty() => parts[0],
            _ => self.query.raw_query.trim(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult<'a> {
    pub items: Vec<&'a FileItem>,
    pub scores: Vec<Score>,
    pub total_matched: usize,
    pub total_files: usize,
    pub location: Option<Location>,
}

const MAX_MMAP_FILE_SIZE: u64 = 10 * 1024 * 1024;

// Limits the total number of files (and bytes) whose content is kept in
// memory via the `OnceLock<FileContent>` cache. On Unix every cached file
// holds a live `mmap`, which consumes a kernel `vm_map_entry`. On a 500k-file
// monorepo, caching everything exhausts macOS/Linux kernel resources and
// crashes the machine (see issue #294).
//
// Each `FilePicker` owns its own `ContentCacheBudget`. The budget is passed
// to `grep_search` and `warmup_mmaps` so that multiple pickers can coexist
// without interfering with each other's counters.

const MAX_CACHED_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

/// Per-picker budget controlling how many files may have their content
/// persistently cached (mmap on Unix, heap buffer on Windows).
#[derive(Debug)]
pub struct ContentCacheBudget {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_file_size: u64,
    pub cached_count: AtomicUsize,
    pub cached_bytes: AtomicU64,
}

impl ContentCacheBudget {
    /// No limits — every eligible file is cached. Useful for tests and
    /// short-lived tools that don't need resource protection.
    pub fn unlimited() -> Self {
        Self {
            max_files: usize::MAX,
            max_bytes: u64::MAX,
            max_file_size: MAX_MMAP_FILE_SIZE,
            cached_count: AtomicUsize::new(0),
            cached_bytes: AtomicU64::new(0),
        }
    }

    pub fn zero() -> Self {
        Self {
            max_files: 0,
            max_bytes: 0,
            max_file_size: 0,
            cached_count: AtomicUsize::new(0),
            cached_bytes: AtomicU64::new(0),
        }
    }

    pub fn new_for_repo(file_count: usize) -> Self {
        let max_files = if file_count > 50_000 {
            5_000
        } else if file_count > 10_000 {
            10_000
        } else {
            30_000 // effectively unlimited for small repos
        };

        let max_bytes = if file_count > 50_000 {
            128 * 1024 * 1024 // 128 MB
        } else if file_count > 10_000 {
            256 * 1024 * 1024 // 256 MB
        } else {
            MAX_CACHED_CONTENT_BYTES // 512 MB
        };

        Self {
            max_files,
            max_bytes,
            max_file_size: MAX_MMAP_FILE_SIZE,
            cached_count: AtomicUsize::new(0),
            cached_bytes: AtomicU64::new(0),
        }
    }

    /// Reset the counters. Called when the file index is rebuilt (rescan /
    /// directory change) and all old `FileItem`s are dropped.
    pub fn reset(&self) {
        self.cached_count.store(0, Ordering::Relaxed);
        self.cached_bytes.store(0, Ordering::Relaxed);
    }
}

impl Default for ContentCacheBudget {
    fn default() -> Self {
        Self::new_for_repo(30_000)
    }
}

impl FileItem {
    /// Create a FileItem with a fully functional ChunkedString path.
    ///
    /// Builds a single-file ChunkedPathStore and **leaks** it so the arena
    /// pointer remains valid forever. Only appropriate for tests and short-lived
    /// tools — production code should use `build_chunked_path_store_from_strings`
    /// and `set_path` instead.
    ///
    /// Returns `(item, arena_base)` — callers that need to read path data must
    /// pass the arena pointer to `relative_path(arena)`, `file_name(arena)`, etc.
    #[doc(hidden)]
    pub fn new_for_test(
        rel_path: &str,
        size: u64,
        modified: u64,
        git_status: Option<git2::Status>,
        is_binary: bool,
    ) -> Self {
        let filename_start = rel_path.rfind('/').map(|i| i + 1).unwrap_or(0) as u16;
        let mut item = Self::new_raw(filename_start, size, modified, git_status, is_binary);
        let paths = [rel_path.to_string()];
        let (store, strings) = crate::simd_path::build_chunked_path_store_from_strings(
            &paths,
            std::slice::from_ref(&item),
        );
        let mut cs = strings.into_iter().next().unwrap();
        cs.set_arena_override(store.arena_base_ptr());
        item.set_path(cs);
        // Leak the store so the arena pointer stays valid forever.
        std::mem::forget(store);
        item
    }

    /// Like [`new_for_test`] but also returns the arena base pointer.
    #[doc(hidden)]
    pub fn new_for_test_with_arena(
        rel_path: &str,
        size: u64,
        modified: u64,
        git_status: Option<git2::Status>,
        is_binary: bool,
    ) -> (Self, *const u8) {
        let filename_start = rel_path.rfind('/').map(|i| i + 1).unwrap_or(0) as u16;
        let mut item = Self::new_raw(filename_start, size, modified, git_status, is_binary);
        let paths = [rel_path.to_string()];
        let (store, strings) = crate::simd_path::build_chunked_path_store_from_strings(
            &paths,
            std::slice::from_ref(&item),
        );
        let mut cs = strings.into_iter().next().unwrap();
        let arena = store.arena_base_ptr();
        cs.set_arena_override(arena);
        item.set_path(cs);
        std::mem::forget(store);
        (item, arena)
    }
}
