use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

// -- Wire types (serializable mirrors of fff-core types) --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileItemWire {
    pub relative_path: String,
    pub name: String,
    pub size: u64,
    pub modified: u64,
    pub access_frecency_score: i16,
    pub modification_frecency_score: i16,
    pub total_frecency_score: i32,
    pub git_status: String,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreWire {
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
    pub match_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LocationWire {
    Line(i32),
    Position { line: i32, col: i32 },
    Range { start: (i32, i32), end: (i32, i32) },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultWire {
    pub items: Vec<FileItemWire>,
    pub scores: Vec<ScoreWire>,
    pub total_matched: usize,
    pub total_files: usize,
    pub location: Option<LocationWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchWire {
    pub relative_path: String,
    pub name: String,
    pub is_binary: bool,
    pub git_status: String,
    pub size: u64,
    pub modified: u64,
    pub total_frecency_score: i32,
    pub access_frecency_score: i16,
    pub modification_frecency_score: i16,
    pub line_number: u64,
    pub col: usize,
    pub byte_offset: u64,
    pub line_content: String,
    pub match_ranges: Vec<(u32, u32)>,
    pub fuzzy_score: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepResultWire {
    pub items: Vec<GrepMatchWire>,
    pub total_matched: usize,
    pub total_files_searched: usize,
    pub total_files: usize,
    pub filtered_file_count: usize,
    pub next_file_offset: usize,
    pub regex_fallback_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgressWire {
    pub scanned_files_count: usize,
    pub is_scanning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckWire {
    pub version: String,
    pub git_available: bool,
    pub git_repository_found: bool,
    pub git_libgit2_version: String,
    pub git_workdir: Option<String>,
    pub file_picker_initialized: bool,
    pub file_picker_base_path: Option<String>,
    pub file_picker_is_scanning: Option<bool>,
    pub file_picker_indexed_files: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub enum GrepModeWire {
    #[default]
    PlainText,
    Regex,
    Fuzzy,
}

// -- Request / Response --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Shutdown,

    // Directory management
    IndexDirectory {
        path: PathBuf,
        watch_git_events: bool,
    },
    DropDirectory {
        path: PathBuf,
    },
    ListDirectories,
    WaitForScan {
        path: PathBuf,
        timeout_ms: u64,
    },
    GetScanProgress {
        path: PathBuf,
    },
    IsScanning {
        path: PathBuf,
    },

    // Database
    InitDb {
        frecency_db_path: String,
        history_db_path: String,
        use_unsafe_no_lock: bool,
    },

    // Search
    FuzzySearch {
        path: PathBuf,
        query: String,
        max_threads: usize,
        current_file: Option<String>,
        combo_boost_score_multiplier: i32,
        min_combo_count: u32,
        page_index: usize,
        page_size: usize,
    },
    GrepSearch {
        path: PathBuf,
        query: String,
        file_offset: usize,
        page_size: usize,
        max_file_size: u64,
        max_matches_per_file: usize,
        smart_case: bool,
        grep_mode: GrepModeWire,
        time_budget_ms: u64,
        trim_whitespace: bool,
    },

    // Frecency
    TrackAccess {
        path: PathBuf,
        file_path: String,
    },
    UpdateSingleFileFrecency {
        path: PathBuf,
        file_path: String,
    },

    // Query tracking
    TrackQueryCompletion {
        path: PathBuf,
        query: String,
        file_path: String,
    },
    TrackGrepQuery {
        path: PathBuf,
        query: String,
    },
    GetHistoricalQuery {
        path: PathBuf,
        offset: usize,
    },
    GetHistoricalGrepQuery {
        path: PathBuf,
        offset: usize,
    },

    // Git
    RefreshGitStatus {
        path: PathBuf,
    },
    GetGitRoot {
        path: PathBuf,
    },
    GetBasePath {
        path: PathBuf,
    },

    // Lifecycle
    TriggerRescan {
        path: PathBuf,
    },
    RestartIndex {
        path: PathBuf,
        new_path: PathBuf,
    },
    StopBackgroundMonitor {
        path: PathBuf,
    },
    CleanupFilePicker {
        path: PathBuf,
    },
    DestroyFrecencyDb,
    DestroyQueryDb,

    // Utility
    ParseGrepQuery {
        query: String,
    },
    HealthCheck {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong,
    Ok,
    Bool(bool),
    Error(String),

    SearchResult(SearchResultWire),
    GrepResult(GrepResultWire),
    ScanProgress(ScanProgressWire),
    HealthCheck(HealthCheckWire),
    Directories(Vec<DirectoryInfoWire>),
    OptionalString(Option<String>),
    Usize(usize),
    ParsedGrepQuery { grep_text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryInfoWire {
    pub path: PathBuf,
    pub is_scanning: bool,
    pub file_count: usize,
    pub client_count: usize,
    pub client_pids: Vec<u32>,
}

// -- Framing: length-prefixed MessagePack --

const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MB

pub fn write_frame<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    let len = data.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn encode_request(req: &Request) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec(req)
}

pub fn decode_request(data: &[u8]) -> Result<Request, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

pub fn encode_response(resp: &Response) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec(resp)
}

pub fn decode_response(data: &[u8]) -> Result<Response, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

pub fn socket_path() -> PathBuf {
    if cfg!(target_os = "linux") {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("fff").join("fff.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir()
        .join(format!("fff-{uid}"))
        .join("fff.sock")
}

pub fn pid_file_path() -> PathBuf {
    socket_path().with_extension("pid")
}

pub const DAEMON_BIN_NAME: &str = "fff-daemon";
pub const PROTOCOL_VERSION: u32 = 1;
