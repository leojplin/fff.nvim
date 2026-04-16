use ahash::AHashMap;
use smallvec::SmallVec;

#[derive(Clone, Copy)]
pub struct ArenaPtr(pub *const u8);

// SAFETY: The arena is a read-only immutable part of file sync
unsafe impl Send for ArenaPtr {}
unsafe impl Sync for ArenaPtr {}

impl ArenaPtr {
    #[inline]
    pub fn new(ptr: *const u8) -> Self {
        Self(ptr)
    }

    #[inline]
    pub fn null() -> Self {
        Self(std::ptr::null())
    }

    #[inline]
    pub fn as_ptr(self) -> *const u8 {
        self.0
    }
}

impl std::fmt::Debug for ArenaPtr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ArenaPtr({:?})", self.0)
    }
}

/// 16-byte SIMD-aligned chunk — same width as `uint8x16_t` / `__m128i`.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct SimdChunk([u8; 16]);

impl SimdChunk {
    /// Mutable access to the underlying byte array.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8; 16] {
        &mut self.0
    }
}

impl Default for SimdChunk {
    #[inline]
    fn default() -> Self {
        Self([0u8; 16])
    }
}

impl std::fmt::Debug for SimdChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show the actual bytes, trimming trailing zeros for readability
        let end = self.0.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        write!(f, "SimdChunk({:?})", &self.0[..end])
    }
}

/// Stack buffer size for path operations. Covers any realistic relative path
/// (Linux PATH_MAX = 4096). `read_to_buf` truncates safely if exceeded.
pub const PATH_BUF_SIZE: usize = 4096;

/// A string stored as a sequence of indices into a shared `SimdChunk` arena.
///
/// Does NOT hold a pointer to the arena — all methods that read data require
/// an explicit `arena_base: *const u8` parameter obtained from the owning
/// `ChunkedPathStore::arena_base_ptr()`. This makes `ChunkedString` plain
/// data (no raw pointers) so it is automatically `Send + Sync`.
///
/// # Usage
///
/// ```ignore
/// let arena = store.arena_base_ptr();
/// let dir = file.path.dir_str(arena, &mut buf);
/// let fname = file.path.file_name(arena, &mut buf);
/// let ptrs = file.path.resolve_ptrs(arena, &mut ptrs_buf);
/// ```
#[derive(Clone)]
pub struct ChunkedString {
    /// Indices into the chunk arena. Each index `i` refers to the chunk at
    /// `arena_base + i * 16`. Inline for ≤ 4 chunks (64 bytes), heap for more.
    indices: SmallVec<[u32; 4]>,
    /// Actual byte length of the stored string (without zero-padding).
    pub byte_len: u16,
    /// Byte offset where the filename begins. 0 for root-level files.
    /// For dir-only strings, equals `byte_len`.
    pub filename_offset: u16,
    /// Per-item arena override. Null = use the external `arena_base` argument.
    /// Non-null = use this pointer instead (for overflow files with leaked stores).
    arena_override: *const u8,
}

// SAFETY: arena_override is either null or points to a leaked (permanently live)
// allocation that is never mutated. Multiple threads reading the same immutable
// data through different ChunkedStrings is safe.
unsafe impl Send for ChunkedString {}
unsafe impl Sync for ChunkedString {}

impl ChunkedString {
    /// Empty placeholder used during walk phase before the arena is built.
    pub fn empty() -> Self {
        Self {
            indices: SmallVec::new(),
            byte_len: 0,
            filename_offset: 0,
            arena_override: std::ptr::null(),
        }
    }

    /// Create a new ChunkedString.
    #[inline]
    pub fn new(indices: SmallVec<[u32; 4]>, byte_len: u16, filename_offset: u16) -> Self {
        Self {
            indices,
            byte_len,
            filename_offset,
            arena_override: std::ptr::null(),
        }
    }

    /// Set a per-item arena override (for overflow files with leaked stores).
    #[inline]
    pub fn set_arena_override(&mut self, ptr: *const u8) {
        self.arena_override = ptr;
    }

    /// Resolve the effective arena pointer: use the override if set, otherwise
    /// fall back to the caller-provided arena.
    #[inline]
    fn effective_arena(&self, arena_base: *const u8) -> *const u8 {
        if self.arena_override.is_null() {
            arena_base
        } else {
            self.arena_override
        }
    }

    /// Number of 16-byte chunks.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.indices.len()
    }

    /// Resolve chunk pointers onto a stack buffer for SIMD matching.
    /// Returns the slice of valid pointers.
    #[inline]
    pub fn resolve_ptrs<'a>(
        &self,
        arena_base: *const u8,
        buf: &'a mut [*const u8; 32],
    ) -> &'a [*const u8] {
        let arena_base = self.effective_arena(arena_base);
        let count = self.indices.len();
        for (i, &idx) in self.indices.iter().enumerate() {
            buf[i] = unsafe { arena_base.add(idx as usize * 16) };
        }
        &buf[..count]
    }

    /// Read the full string into a caller-provided buffer.
    /// If the path exceeds `buf.len()`, it is silently truncated at a chunk
    /// boundary — callers should use `[u8; PATH_BUF_SIZE]` to avoid this.
    #[inline]
    pub fn read_to_buf<'a>(&self, arena_base: *const u8, buf: &'a mut [u8]) -> &'a str {
        let arena_base = self.effective_arena(arena_base);
        let total = (self.byte_len as usize).min(buf.len());
        let usable_chunks = total.div_ceil(16);
        let chunks_to_copy = usable_chunks.min(self.indices.len());
        for (i, &idx) in self.indices[..chunks_to_copy].iter().enumerate() {
            let src = unsafe { arena_base.add(idx as usize * 16) };
            let dst_offset = i * 16;
            let take = 16.min(total - dst_offset);
            unsafe {
                core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr().add(dst_offset), take);
            }
        }
        unsafe { core::str::from_utf8_unchecked(&buf[..total]) }
    }

    /// Read the directory portion `[0..filename_offset]`.
    #[inline]
    pub fn dir_str<'a>(&self, arena_base: *const u8, buf: &'a mut [u8]) -> &'a str {
        let arena_base = self.effective_arena(arena_base);
        let dir_len = (self.filename_offset as usize).min(buf.len());
        if dir_len == 0 {
            return "";
        }
        let dir_chunks = chunks_needed(dir_len).min(self.indices.len());
        for (i, &idx) in self.indices[..dir_chunks].iter().enumerate() {
            let src = unsafe { arena_base.add(idx as usize * 16) };
            let dst_offset = i * 16;
            let take = 16.min(dir_len - dst_offset);
            unsafe {
                core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr().add(dst_offset), take);
            }
        }
        unsafe { core::str::from_utf8_unchecked(&buf[..dir_len]) }
    }

    /// Read the filename portion `[filename_offset..byte_len]`.
    #[inline]
    pub fn file_name<'a>(&self, arena_base: *const u8, buf: &'a mut [u8]) -> &'a str {
        let arena_base = self.effective_arena(arena_base);
        let fname_offset = self.filename_offset as usize;
        let total = self.byte_len as usize;
        let fname_len = total - fname_offset;
        if fname_len == 0 {
            return "";
        }
        // Find which chunk the filename starts in
        let start_chunk = fname_offset / 16;
        let offset_in_chunk = fname_offset % 16;
        let needed_chunks = chunks_needed(offset_in_chunk + fname_len);

        // Read chunks into temp, extract filename
        let mut tmp = [0u8; 512];
        for (i, &idx) in self.indices[start_chunk..start_chunk + needed_chunks]
            .iter()
            .enumerate()
        {
            let src = unsafe { arena_base.add(idx as usize * 16) };
            let dst = i * 16;
            let take = 16.min(offset_in_chunk + fname_len - dst);
            unsafe {
                core::ptr::copy_nonoverlapping(src, tmp.as_mut_ptr().add(dst), take);
            }
        }
        buf[..fname_len].copy_from_slice(&tmp[offset_in_chunk..offset_in_chunk + fname_len]);
        unsafe { core::str::from_utf8_unchecked(&buf[..fname_len]) }
    }

    /// Total byte length.
    #[inline]
    pub fn len(&self) -> usize {
        self.byte_len as usize
    }

    /// Whether the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.byte_len == 0
    }

    /// Get the directory portion as an owned String.
    pub fn dir_string(&self, arena_base: *const u8) -> String {
        let mut buf = [0u8; 512];
        self.dir_str(arena_base, &mut buf).to_string()
    }

    /// Get the filename as an owned String.
    pub fn file_name_string(&self, arena_base: *const u8) -> String {
        let mut buf = [0u8; 512];
        self.file_name(arena_base, &mut buf).to_string()
    }

    /// Get the full relative path as an owned String.
    pub fn to_string(&self, arena_base: *const u8) -> String {
        let mut buf = [0u8; 512];
        self.read_to_buf(arena_base, &mut buf).to_string()
    }
}

impl std::fmt::Debug for ChunkedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkedString")
            .field("indices", &self.indices.as_slice())
            .field("chunks", &self.indices.len())
            .field("byte_len", &self.byte_len)
            .field("filename_offset", &self.filename_offset)
            .finish()
    }
}

/// Number of 16-byte chunks needed to store `byte_len` bytes.
/// Returns 0 for empty input (root-level directory).
#[inline]
const fn chunks_needed(byte_len: usize) -> usize {
    if byte_len == 0 {
        0
    } else {
        byte_len.div_ceil(16)
    }
}

/// Frozen chunk arena. After `ChunkedPathStoreBuilder::finish()`, this holds
/// only the deduped 16-byte chunks. All per-file metadata lives in the
/// `ChunkedString`s that were created inline via `add_file_immediate()`.
#[derive(Clone)]
pub struct ChunkedPathStore {
    /// Deduped 16-byte aligned chunks. Each unique 16-byte block gets one slot.
    arena: Vec<SimdChunk>,
}

// SAFETY: arena is immutable after construction. Pointers derived from it are
// only read during scoring (no mutation, no reallocation).
unsafe impl Send for ChunkedPathStore {}
unsafe impl Sync for ChunkedPathStore {}

impl ChunkedPathStore {
    /// Total heap bytes used by this store (arena only).
    pub fn heap_bytes(&self) -> usize {
        self.arena.len() * 16
    }

    /// Number of unique chunks in the arena.
    pub fn unique_chunks(&self) -> usize {
        self.arena.len()
    }

    /// Get the arena base pointer.
    #[inline]
    pub fn arena_base_ptr(&self) -> *const u8 {
        self.arena.as_ptr() as *const u8
    }
}

impl std::fmt::Debug for ChunkedPathStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkedPathStore")
            .field("unique_chunks", &self.arena.len())
            .field("heap_bytes", &self.heap_bytes())
            .finish()
    }
}

/// Incremental builder for `ChunkedPathStore`.
///
/// Feed paths one at a time via `add_file_immediate()`. Each call chunks and
/// deduplicates the path inline and returns a `ChunkedString`. Call `finish()`
/// to freeze the arena and produce the final store.
pub struct ChunkedPathStoreBuilder {
    arena: Vec<SimdChunk>,
    chunk_dedup: AHashMap<[u8; 16], u32>,
    dir_dedup: AHashMap<String, ()>,
}

impl ChunkedPathStoreBuilder {
    /// Create a new builder with estimated capacity.
    pub fn new(estimated_files: usize) -> Self {
        let est_chunks = estimated_files * 3;
        Self {
            arena: Vec::with_capacity(est_chunks / 2),
            chunk_dedup: AHashMap::with_capacity(est_chunks / 2),
            dir_dedup: AHashMap::new(),
        }
    }

    /// Freeze the arena and produce the final `ChunkedPathStore`.
    pub fn finish(self) -> ChunkedPathStore {
        ChunkedPathStore { arena: self.arena }
    }

    /// Add a file and return a `ChunkedString`. The ChunkedString stores
    /// only indices — the caller must obtain `arena_base_ptr()` from the
    /// finished store and pass it when reading data.
    pub fn add_file_immediate(&mut self, rel_path: &str, filename_offset: u16) -> ChunkedString {
        let dir_part = &rel_path[..filename_offset as usize];

        // Ensure directory chunks exist in the arena (dedup across files
        // sharing the same parent directory).
        if !dir_part.is_empty() && !self.dir_dedup.contains_key(dir_part) {
            let dir_bytes = dir_part.as_bytes();
            let dir_len = dir_part.len();
            let n_dir_chunks = chunks_needed(dir_len);
            for i in 0..n_dir_chunks {
                let chunk_start = i * 16;
                let chunk_end = (chunk_start + 16).min(dir_len);
                let mut chunk_bytes = [0u8; 16];
                chunk_bytes[..chunk_end - chunk_start]
                    .copy_from_slice(&dir_bytes[chunk_start..chunk_end]);

                if !self.chunk_dedup.contains_key(&chunk_bytes) {
                    let idx = self.arena.len() as u32;
                    self.arena.push(SimdChunk(chunk_bytes));
                    self.chunk_dedup.insert(chunk_bytes, idx);
                }
            }
            self.dir_dedup.insert(dir_part.to_string(), ());
        }

        // Chunk the full path and collect indices
        let path_bytes = rel_path.as_bytes();
        let byte_len = rel_path.len();
        let n_chunks = chunks_needed(byte_len);
        let mut indices = SmallVec::<[u32; 4]>::with_capacity(n_chunks);

        for i in 0..n_chunks {
            let chunk_start = i * 16;
            let chunk_end = (chunk_start + 16).min(byte_len);
            let mut chunk_bytes = [0u8; 16];
            chunk_bytes[..chunk_end - chunk_start]
                .copy_from_slice(&path_bytes[chunk_start..chunk_end]);

            let arena_idx = match self.chunk_dedup.get(&chunk_bytes) {
                Some(&idx) => idx,
                None => {
                    let idx = self.arena.len() as u32;
                    self.arena.push(SimdChunk(chunk_bytes));
                    self.chunk_dedup.insert(chunk_bytes, idx);
                    idx
                }
            };
            indices.push(arena_idx);
        }

        ChunkedString::new(indices, byte_len as u16, filename_offset)
    }
}

/// Build a `ChunkedPathStore` from parallel slices of paths and FileItems.
/// Returns `(store, chunked_strings)` — callers assign each `ChunkedString`
/// to the corresponding `FileItem` via `set_path`.
pub fn build_chunked_path_store_from_strings(
    rel_paths: &[String],
    files: &[crate::types::FileItem],
) -> (ChunkedPathStore, Vec<ChunkedString>) {
    assert_eq!(rel_paths.len(), files.len());
    let mut builder = ChunkedPathStoreBuilder::new(rel_paths.len());
    let strings: Vec<ChunkedString> = rel_paths
        .iter()
        .zip(files.iter())
        .map(|(rel_path, file)| builder.add_file_immediate(rel_path, file.path.filename_offset))
        .collect();
    (builder.finish(), strings)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file_item(path: &str) -> crate::types::FileItem {
        let filename_start = path.rfind('/').map(|i| i + 1).unwrap_or(0) as u16;
        crate::types::FileItem::new_raw(filename_start, 0, 0, None, false)
    }

    fn build_test_store(
        paths: &[&str],
    ) -> (
        ChunkedPathStore,
        Vec<ChunkedString>,
        Vec<crate::types::FileItem>,
    ) {
        let mut files: Vec<crate::types::FileItem> =
            paths.iter().map(|p| make_file_item(p)).collect();
        let path_strings: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let (store, strings) = build_chunked_path_store_from_strings(&path_strings, &files);
        for (i, file) in files.iter_mut().enumerate() {
            file.set_path(strings[i].clone());
        }
        (store, strings, files)
    }

    #[test]
    fn test_chunked_store_empty() {
        let (store, strings, _files) = build_test_store(&[]);
        assert_eq!(strings.len(), 0);
        assert_eq!(store.unique_chunks(), 0);
    }

    #[test]
    fn test_chunked_store_basic() {
        let (store, strings, _files) =
            build_test_store(&["src/lib.rs", "src/main.rs", "Cargo.toml"]);
        let arena = store.arena_base_ptr();

        assert_eq!(strings.len(), 3);
        assert!(store.unique_chunks() >= 2);

        let mut buf = [0u8; 512];
        assert_eq!(
            strings[0].read_to_buf(arena, &mut buf).len(),
            "src/lib.rs".len()
        );
        assert_eq!(
            strings[2].read_to_buf(arena, &mut buf).len(),
            "Cargo.toml".len()
        );
    }

    #[test]
    fn test_chunked_string_full_path() {
        let (store, strings, _files) = build_test_store(&["src/components/Button.tsx"]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];

        let mut buf = [0u8; 512];
        assert_eq!(cs.read_to_buf(arena, &mut buf), "src/components/Button.tsx");
        assert_eq!(cs.byte_len, 25);
        assert_eq!(cs.filename_offset, 15);
    }

    #[test]
    fn test_chunked_string_dir_and_filename() {
        let (store, strings, _files) = build_test_store(&["src/components/Button.tsx"]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];

        let mut buf = [0u8; 512];
        assert_eq!(cs.dir_str(arena, &mut buf), "src/components/");
        let mut buf2 = [0u8; 512];
        assert_eq!(cs.file_name(arena, &mut buf2), "Button.tsx");
    }

    #[test]
    fn test_chunked_string_root_file() {
        let (store, strings, _files) = build_test_store(&["Cargo.toml"]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];

        let mut buf = [0u8; 512];
        assert_eq!(cs.dir_str(arena, &mut buf), "");
        let mut buf2 = [0u8; 512];
        assert_eq!(cs.file_name(arena, &mut buf2), "Cargo.toml");
        let mut buf3 = [0u8; 512];
        assert_eq!(cs.read_to_buf(arena, &mut buf3), "Cargo.toml");
    }

    #[test]
    fn test_chunked_string_resolve_ptrs() {
        let (store, strings, _files) = build_test_store(&["src/components/Button.tsx"]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];

        let mut ptrs = [std::ptr::null::<u8>(); 32];
        let resolved = cs.resolve_ptrs(arena, &mut ptrs);
        assert_eq!(resolved.len(), 2); // 25 bytes = 2 chunks

        // Verify we can read back the bytes
        let mut reconstructed = Vec::new();
        for (i, &ptr) in resolved.iter().enumerate() {
            let chunk = unsafe { std::slice::from_raw_parts(ptr, 16) };
            let start = i * 16;
            let take = 16.min(25 - start);
            reconstructed.extend_from_slice(&chunk[..take]);
        }
        assert_eq!(
            std::str::from_utf8(&reconstructed).unwrap(),
            "src/components/Button.tsx"
        );
    }

    #[test]
    fn test_chunked_string_long_path() {
        let path = "very/deeply/nested/directory/structure/with/many/levels/file.txt";
        let (store, strings, _files) = build_test_store(&[path]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];

        let mut buf = [0u8; 512];
        assert_eq!(cs.read_to_buf(arena, &mut buf), path);
        assert!(
            cs.chunk_count() <= 6,
            "should fit inline in SmallVec<[u32; 4]>"
        );
    }

    #[test]
    fn test_chunked_string_clone() {
        let (store, strings, _files) = build_test_store(&["src/main.rs"]);
        let arena = store.arena_base_ptr();
        let cs = &strings[0];
        let cs2 = cs.clone();

        let mut buf1 = [0u8; 512];
        let mut buf2 = [0u8; 512];
        assert_eq!(
            cs.read_to_buf(arena, &mut buf1),
            cs2.read_to_buf(arena, &mut buf2)
        );
    }

    #[test]
    fn test_chunked_string_full_path_roundtrip() {
        let paths = [
            "src/components/Button.tsx",
            "src/components/ui/DatePicker.tsx",
            "very/deeply/nested/directory/structure/file.txt",
            "Cargo.toml",
            "a.rs",
        ];
        let (store, strings, _files) = build_test_store(&paths);
        let arena = store.arena_base_ptr();

        for (i, expected) in paths.iter().enumerate() {
            let mut buf = [0u8; 512];
            let got = strings[i].read_to_buf(arena, &mut buf);
            assert_eq!(got, *expected, "full path roundtrip failed for file {i}");

            let mut dbuf = [0u8; 512];
            let mut fbuf = [0u8; 512];
            let dir = strings[i].dir_str(arena, &mut dbuf);
            let fname = strings[i].file_name(arena, &mut fbuf);
            assert_eq!(
                format!("{dir}{fname}"),
                *expected,
                "dir+fname mismatch for file {i}"
            );
        }
    }
}
