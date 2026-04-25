# FFF Daemon Feasibility Analysis and Design

## Problem Statement

Multiple processes (Neovim instances, MCP servers, editor plugins, CLI tools) often need to search files in the same directory. Each currently spawns its own fff instance, paying:

- **CPU:** Filesystem walk, git status collection, bigram index build — all duplicated per instance
- **Memory:** `Vec<FileItem>`, `ChunkedPathStore`, `BigramFilter`, content cache — all duplicated
- **I/O:** Each instance walks the full directory tree, opens the same files for content indexing
- **Latency:** Each instance waits for its own initial scan before returning results

A daemon eliminates this N-fold duplication.

---

## Feasibility Assessment: HIGH

The existing architecture is nearly daemon-ready:

### What already exists

1. **`SharedPicker(Arc<parking_lot::RwLock<Option<FilePicker>>>)`** — the engine is already behind a concurrent reader-writer lock designed for multi-threaded access
2. **`fff-mcp` already runs as a standalone binary** using fff-core directly (no Neovim dependency), with its own `main()`, CLI args, and async runtime
3. **`fff-c` already exposes a C ABI** with opaque handles and accessor functions
4. **All state is managed inside `fff-core`** — no frontend (Neovim, C, MCP) owns any search state
5. **The `BackgroundWatcher` already handles concurrent read/write** — filesystem events update the index while searches read it
6. **LMDB databases are already multi-process safe** — `heed`/LMDB supports concurrent readers across processes natively

### What needs to be built

1. An IPC transport between clients and the daemon
2. A daemon lifecycle manager (auto-start, discovery, graceful shutdown)
3. A client library that replaces direct `fff-core` calls with IPC requests
4. Adaptation of `fff-nvim` to use the client library instead of in-process calls

---

## Architecture Decision: Single Global Daemon

### Decision: One daemon per user, serving all paths

**Rejected alternative:** One daemon per directory path.

**Rationale:**

1. **Process overhead:** Even a lightweight daemon consumes ~8 MB RSS baseline (Rust runtime + tokio). Users working in 5-10 projects would have 5-10 daemon processes, each with its own thread pools. A single daemon shares one rayon pool, one tokio runtime, one mimalloc heap.

2. **The `FilePicker` already supports reindexing:** `change_indexing_directory()` exists. The daemon can maintain a `HashMap<PathBuf, SharedPicker>` — one picker per active directory — while sharing all infrastructure.

3. **Cross-project search:** A single daemon can support queries that span multiple indexed directories (future capability).

4. **Lifecycle simplicity:** One PID file, one socket, one health check. Per-directory daemons need a registry, per-directory socket files, and cleanup logic for stale sockets.

5. **Memory sharing:** OS page cache is global. If two pickers index overlapping files (monorepo subdirectories), their mmaps share physical pages automatically. A single daemon makes this transparent.

**Trade-off:** A single daemon with 10 indexed directories uses more memory than a per-directory daemon serving just one. Mitigation: LRU eviction of inactive directory indexes (configurable idle timeout).

---

## Daemon Design

### Transport: Unix Domain Socket (macOS/Linux) + Named Pipe (Windows)

**Why not TCP:** File search is local. UDS has lower overhead (no TCP handshake, no Nagle, no port allocation), filesystem-based permissions, and natural lifecycle (socket file deletion = daemon gone).

**Why not shared memory:** The search API is request-response, not streaming. The overhead of UDS for a serialized response is negligible compared to the search itself. Shared memory would add complexity (synchronization, fixed layouts) for minimal gain.

**Protocol:** Length-prefixed MessagePack frames over the socket. MessagePack is compact, fast to serialize, and has excellent Rust (`rmp-serde`) and Lua (`msgpack.lua` / LuaJIT FFI) support. Neovim already uses MessagePack for its own RPC protocol.

```
Frame: [4-byte big-endian length][MessagePack payload]
```

### Socket Location

```
$XDG_RUNTIME_DIR/fff/fff.sock       (Linux, typically /run/user/$UID/fff/)
$TMPDIR/fff-$UID/fff.sock           (macOS fallback)
\\.\pipe\fff-{username}              (Windows named pipe)
```

### API Surface

```rust
enum Request {
    // Lifecycle
    Ping,
    Shutdown,

    // Directory management
    IndexDirectory { path: PathBuf, options: IndexOptions },
    DropDirectory { path: PathBuf },
    ListDirectories,
    WaitForScan { path: PathBuf, timeout_ms: u64 },
    GetScanProgress { path: PathBuf },

    // Search
    FuzzySearch { path: PathBuf, query: String, options: FuzzySearchOptions },
    DirSearch { path: PathBuf, query: String, options: DirSearchOptions },
    GrepSearch { path: PathBuf, query: String, options: GrepSearchOptions },
    MultiGrep { path: PathBuf, patterns: Vec<String>, options: GrepSearchOptions },

    // Frecency
    TrackAccess { path: PathBuf, file_path: String },
    TrackQueryCompletion { path: PathBuf, query: String, file_path: String },
    GetHistoricalQueries { path: PathBuf, mode: QueryMode, offset: i32 },

    // Git
    RefreshGitStatus { path: PathBuf },

    // Health
    HealthCheck,
}

enum Response {
    Ok,
    Error { message: String },
    Pong,
    SearchResult { items: Vec<SearchResultItem>, total_matched: u64, ... },
    GrepResult { items: Vec<GrepMatchItem>, total_matched: u64, ... },
    ScanProgress { scanned: u64, total: Option<u64>, is_complete: bool },
    DirectoryList { directories: Vec<DirectoryInfo> },
    HealthReport { ... },
}
```

### Daemon Lifecycle

```
Client wants to search:
  1. Try connect to $SOCKET_PATH
  2. If connection refused or socket missing:
     a. Fork/exec `fff-daemon` (the daemon binary)
     b. Daemon writes PID file, binds socket, enters event loop
     c. Client retries connect (up to 5 attempts, 100ms backoff)
  3. Send IndexDirectory if this path isn't yet indexed
  4. Send search requests

Daemon auto-shutdown:
  - Configurable idle timeout (default: 30 minutes)
  - No connected clients + no active indexes → graceful shutdown
  - Signal handlers: SIGTERM → graceful, SIGINT → graceful, SIGHUP → reload config

Stale socket detection:
  - On connect failure: check PID file → kill(pid, 0) → if process dead, unlink socket + PID file → spawn new daemon
```

### Directory Index LRU

```rust
struct DaemonState {
    directories: HashMap<PathBuf, DirectoryEntry>,
    lru_order: VecDeque<PathBuf>,
    max_directories: usize,  // default: 20
    idle_timeout: Duration,  // default: 30 min per directory
}

struct DirectoryEntry {
    picker: SharedPicker,
    frecency: SharedFrecency,
    query_tracker: SharedQueryTracker,
    last_accessed: Instant,
    client_count: AtomicUsize,  // number of clients using this directory
}
```

Directories with `client_count == 0` and `last_accessed > idle_timeout` are eligible for eviction. Active directories (any client connected) are never evicted.

---

## Implementation Plan

### Phase 1: fff-daemon binary (new crate: `crates/fff-daemon`)

```toml
[dependencies]
fff = { package = "fff-search", path = "../fff-core" }
tokio = { version = "1", features = ["full"] }
rmp-serde = "1"
serde = { version = "1", features = ["derive"] }
clap = { version = "4", features = ["derive"] }
mimalloc = "0.1"
tracing = "0.1"
tracing-subscriber = "0.3"
```

**Deliverables:**
- `src/main.rs` — CLI args, daemon fork, signal handling, PID file
- `src/protocol.rs` — `Request`/`Response` enums, MessagePack serialization
- `src/server.rs` — tokio accept loop, per-connection handler, dispatch to fff-core
- `src/state.rs` — `DaemonState` with directory LRU management
- `src/lifecycle.rs` — idle timeout, graceful shutdown, stale socket cleanup

### Phase 2: fff-client library (new crate: `crates/fff-client`)

```toml
[dependencies]
rmp-serde = "1"
serde = { version = "1", features = ["derive"] }
```

**Deliverables:**
- `src/lib.rs` — `FffClient` struct: connect, auto-spawn daemon, send/recv frames
- `src/protocol.rs` — shared `Request`/`Response` types (shared with daemon via workspace dep)
- Synchronous API matching the current `fff-core` public API surface (drop-in replacement for callers)

### Phase 3: Adapt fff-nvim to use daemon

**Changes to `crates/fff-nvim/src/lib.rs`:**
- Replace `Lazy<SharedPicker>` with `Lazy<FffClient>`
- Each Lua-exported function sends a request via `FffClient` instead of calling `SharedPicker` methods directly
- Connection is established on first call (lazy), daemon auto-spawned if needed

**Changes to Lua layer:**
- Minimal — the Rust→Lua API surface stays the same. The Lua code doesn't know whether results come from in-process or daemon.
- `conf.lua`: add `daemon.enabled` (default: `true`), `daemon.socket_path`, `daemon.idle_timeout`, `daemon.auto_spawn`
- `core.lua`: `ensure_initialized()` connects to daemon instead of calling `init_file_picker`

### Phase 4: Adapt fff-mcp to use daemon

**Changes to `crates/fff-mcp/src/main.rs`:**
- Option: `--daemon` flag to connect to running daemon instead of spawning own `FilePicker`
- Default: still standalone (backward compatible)
- When `--daemon`: `FffClient::connect()` → send `IndexDirectory` → use client API for all searches

---

## Performance Impact

### Costs introduced by daemonization

| Cost | Magnitude | Mitigation |
|------|-----------|------------|
| IPC serialization (MessagePack) | ~50-200 μs per search | Negligible vs search time (1-10 ms) |
| Socket round-trip | ~10-50 μs (UDS) | Single round-trip per search |
| Daemon memory overhead | ~8 MB base | Shared across all clients |
| Connection establishment | ~1 ms first time | Persistent connections, lazy init |

### Savings from daemonization

| Saving | Per-instance cost eliminated | At 3 instances |
|--------|----------------------------|----------------|
| Initial filesystem scan | 200-2000 ms + I/O | 400-4000 ms saved |
| Memory (50K file project) | ~80 MB (FileItems + arena + bigrams + cache) | ~160 MB saved |
| Git status collection | 50-500 ms | 100-1000 ms saved |
| Bigram index build | 100-500 ms CPU | 200-1000 ms saved |
| Rayon thread pool | N threads × stack size | 2N threads eliminated |

**Net result:** First client pays the full startup cost. Second and subsequent clients start searching immediately with zero warmup.

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Daemon crash loses all clients | Low | High | Clients detect broken pipe, auto-restart daemon, re-index |
| Socket permission issues | Medium | Medium | Create socket in user-owned runtime dir, validate permissions |
| Stale daemon after system sleep | Medium | Low | Health check on reconnect, auto-restart if unresponsive |
| Version mismatch (client ≠ daemon) | Medium | Medium | Version handshake on connect, daemon auto-restarts if client is newer |
| Windows named pipe complexity | Medium | Medium | Phase 1 targets Unix only; Windows support in Phase 2 |

---

## Success Criteria

1. **fff standalone:** `fff-core` usable without any editor dependency (already true)
2. **Daemonized:** `fff-daemon` binary that indexes directories and serves search requests over UDS
3. **Auto-spawn:** First client to connect auto-starts the daemon if not running
4. **Attach:** Multiple processes (Neovim instances, MCP servers, CLI tools) connect to the same daemon for the same directory, sharing index and cache
5. **Nvim integration:** `fff-nvim` transparently uses the daemon, with no user-visible behavior change except faster startup on second+ instance
