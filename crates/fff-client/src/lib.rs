use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fff_protocol::*;

pub struct FffClient {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    base_path: PathBuf,
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Encode(rmp_serde::encode::Error),
    Decode(rmp_serde::decode::Error),
    DaemonError(String),
    SpawnFailed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io error: {e}"),
            ClientError::Encode(e) => write!(f, "encode error: {e}"),
            ClientError::Decode(e) => write!(f, "decode error: {e}"),
            ClientError::DaemonError(e) => write!(f, "daemon error: {e}"),
            ClientError::SpawnFailed(e) => write!(f, "spawn failed: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<rmp_serde::encode::Error> for ClientError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        ClientError::Encode(e)
    }
}

impl From<rmp_serde::decode::Error> for ClientError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        ClientError::Decode(e)
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

impl FffClient {
    pub fn connect(base_path: &Path) -> Result<Self> {
        let sock = socket_path();
        Self::connect_to(base_path, &sock)
    }

    pub fn connect_to(base_path: &Path, socket_path: &Path) -> Result<Self> {
        let stream = match UnixStream::connect(socket_path) {
            Ok(s) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Self::spawn_daemon(socket_path)?;
                Self::connect_with_retry(socket_path, 10, Duration::from_millis(200))?
            }
            Err(e) => return Err(e.into()),
        };

        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let reader = BufReader::new(stream.try_clone()?);
        let writer = BufWriter::new(stream);

        Ok(Self {
            reader,
            writer,
            base_path: base_path.to_path_buf(),
        })
    }

    fn spawn_daemon(socket_path: &Path) -> Result<()> {
        // Check if daemon binary is on PATH or next to the client
        let daemon_bin = Self::find_daemon_binary()
            .ok_or_else(|| ClientError::SpawnFailed("fff-daemon binary not found".into()))?;

        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ClientError::SpawnFailed(format!("failed to create socket dir: {e}")))?;
        }

        let mut cmd = Command::new(&daemon_bin);
        cmd.arg("--foreground")
            .arg("--socket")
            .arg(socket_path);

        // Detach the daemon process
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                ClientError::SpawnFailed(format!("failed to spawn {}: {e}", daemon_bin.display()))
            })?;

        Ok(())
    }

    fn find_daemon_binary() -> Option<PathBuf> {
        // Explicit override via environment variable (set by the Neovim plugin's Lua layer)
        if let Ok(explicit) = std::env::var("FFF_DAEMON_BIN") {
            let path = PathBuf::from(&explicit);
            if path.exists() {
                return Some(path);
            }
        }

        // Check next to the current executable
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(DAEMON_BIN_NAME);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        // Check PATH directories
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                let candidate = PathBuf::from(dir).join(DAEMON_BIN_NAME);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        // Check common development locations
        let cargo_target = std::env::var("CARGO_TARGET_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target"));
        let dev_path = cargo_target.join("release").join(DAEMON_BIN_NAME);
        if dev_path.exists() {
            return Some(dev_path);
        }
        let dev_path = cargo_target.join("debug").join(DAEMON_BIN_NAME);
        if dev_path.exists() {
            return Some(dev_path);
        }

        None
    }

    fn connect_with_retry(
        socket_path: &Path,
        max_attempts: usize,
        delay: Duration,
    ) -> io::Result<UnixStream> {
        for attempt in 0..max_attempts {
            std::thread::sleep(delay);
            match UnixStream::connect(socket_path) {
                Ok(s) => return Ok(s),
                Err(e) if attempt == max_attempts - 1 => return Err(e),
                Err(_) => continue,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "daemon did not start in time",
        ))
    }

    fn send(&mut self, request: &Request) -> Result<Response> {
        let data = encode_request(request)?;
        write_frame(&mut self.writer, &data)?;
        let resp_data = read_frame(&mut self.reader)?;
        let response = decode_response(&resp_data)?;
        if let Response::Error(ref msg) = response {
            return Err(ClientError::DaemonError(msg.clone()));
        }
        Ok(response)
    }

    // -- Public API matching fff-nvim's Lua-callable functions --

    pub fn init_db(
        &mut self,
        frecency_db_path: &str,
        history_db_path: &str,
        use_unsafe_no_lock: bool,
    ) -> Result<bool> {
        let resp = self.send(&Request::InitDb {
            frecency_db_path: frecency_db_path.to_string(),
            history_db_path: history_db_path.to_string(),
            use_unsafe_no_lock,
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn init_file_picker(
        &mut self,
        base_path: &str,
        watch_git_events: bool,
    ) -> Result<bool> {
        self.base_path = PathBuf::from(base_path);
        let resp = self.send(&Request::IndexDirectory {
            path: PathBuf::from(base_path),
            watch_git_events,
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn restart_index_in_path(&mut self, new_path: &str) -> Result<()> {
        let old_path = self.base_path.clone();
        self.base_path = PathBuf::from(new_path);
        self.send(&Request::RestartIndex {
            path: old_path,
            new_path: PathBuf::from(new_path),
        })?;
        Ok(())
    }

    pub fn scan_files(&mut self) -> Result<()> {
        self.send(&Request::TriggerRescan {
            path: self.base_path.clone(),
        })?;
        Ok(())
    }

    pub fn fuzzy_search_files(
        &mut self,
        query: &str,
        max_threads: usize,
        current_file: Option<&str>,
        combo_boost_score_multiplier: i32,
        min_combo_count: u32,
        page_index: usize,
        page_size: usize,
    ) -> Result<SearchResultWire> {
        let resp = self.send(&Request::FuzzySearch {
            path: self.base_path.clone(),
            query: query.to_string(),
            max_threads,
            current_file: current_file.map(String::from),
            combo_boost_score_multiplier,
            min_combo_count,
            page_index,
            page_size,
        })?;
        match resp {
            Response::SearchResult(r) => Ok(r),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn live_grep(
        &mut self,
        query: &str,
        file_offset: usize,
        page_size: usize,
        max_file_size: u64,
        max_matches_per_file: usize,
        smart_case: bool,
        grep_mode: GrepModeWire,
        time_budget_ms: u64,
        trim_whitespace: bool,
    ) -> Result<GrepResultWire> {
        let resp = self.send(&Request::GrepSearch {
            path: self.base_path.clone(),
            query: query.to_string(),
            file_offset,
            page_size,
            max_file_size,
            max_matches_per_file,
            smart_case,
            grep_mode,
            time_budget_ms,
            trim_whitespace,
        })?;
        match resp {
            Response::GrepResult(r) => Ok(r),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn track_access(&mut self, file_path: &str) -> Result<bool> {
        let resp = self.send(&Request::TrackAccess {
            path: self.base_path.clone(),
            file_path: file_path.to_string(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn get_scan_progress(&mut self) -> Result<ScanProgressWire> {
        let resp = self.send(&Request::GetScanProgress {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::ScanProgress(p) => Ok(p),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn is_scanning(&mut self) -> Result<bool> {
        let resp = self.send(&Request::IsScanning {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(false),
        }
    }

    pub fn get_git_root(&mut self) -> Result<Option<String>> {
        let resp = self.send(&Request::GetGitRoot {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::OptionalString(s) => Ok(s),
            _ => Ok(None),
        }
    }

    pub fn get_base_path(&mut self) -> Result<Option<String>> {
        let resp = self.send(&Request::GetBasePath {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::OptionalString(s) => Ok(s),
            _ => Ok(None),
        }
    }

    pub fn refresh_git_status(&mut self) -> Result<usize> {
        let resp = self.send(&Request::RefreshGitStatus {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::Usize(n) => Ok(n),
            _ => Ok(0),
        }
    }

    pub fn update_single_file_frecency(&mut self, file_path: &str) -> Result<bool> {
        let resp = self.send(&Request::UpdateSingleFileFrecency {
            path: self.base_path.clone(),
            file_path: file_path.to_string(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn stop_background_monitor(&mut self) -> Result<bool> {
        let resp = self.send(&Request::StopBackgroundMonitor {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn cleanup_file_picker(&mut self) -> Result<bool> {
        let resp = self.send(&Request::CleanupFilePicker {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn wait_for_initial_scan(&mut self, timeout_ms: u64) -> Result<bool> {
        let resp = self.send(&Request::WaitForScan {
            path: self.base_path.clone(),
            timeout_ms,
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn track_query_completion(&mut self, query: &str, file_path: &str) -> Result<bool> {
        let resp = self.send(&Request::TrackQueryCompletion {
            path: self.base_path.clone(),
            query: query.to_string(),
            file_path: file_path.to_string(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn track_grep_query(&mut self, query: &str) -> Result<bool> {
        let resp = self.send(&Request::TrackGrepQuery {
            path: self.base_path.clone(),
            query: query.to_string(),
        })?;
        match resp {
            Response::Bool(b) => Ok(b),
            _ => Ok(true),
        }
    }

    pub fn get_historical_query(&mut self, offset: usize) -> Result<Option<String>> {
        let resp = self.send(&Request::GetHistoricalQuery {
            path: self.base_path.clone(),
            offset,
        })?;
        match resp {
            Response::OptionalString(s) => Ok(s),
            _ => Ok(None),
        }
    }

    pub fn get_historical_grep_query(&mut self, offset: usize) -> Result<Option<String>> {
        let resp = self.send(&Request::GetHistoricalGrepQuery {
            path: self.base_path.clone(),
            offset,
        })?;
        match resp {
            Response::OptionalString(s) => Ok(s),
            _ => Ok(None),
        }
    }

    pub fn parse_grep_query(&mut self, query: &str) -> Result<String> {
        let resp = self.send(&Request::ParseGrepQuery {
            query: query.to_string(),
        })?;
        match resp {
            Response::ParsedGrepQuery { grep_text } => Ok(grep_text),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn health_check(&mut self) -> Result<HealthCheckWire> {
        let resp = self.send(&Request::HealthCheck {
            path: self.base_path.clone(),
        })?;
        match resp {
            Response::HealthCheck(h) => Ok(h),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn list_directories(&mut self) -> Result<Vec<DirectoryInfoWire>> {
        let resp = self.send(&Request::ListDirectories)?;
        match resp {
            Response::Directories(dirs) => Ok(dirs),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn ping(&mut self) -> Result<()> {
        let resp = self.send(&Request::Ping)?;
        match resp {
            Response::Pong => Ok(()),
            other => Err(ClientError::DaemonError(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(&Request::Shutdown);
        Ok(())
    }
}

