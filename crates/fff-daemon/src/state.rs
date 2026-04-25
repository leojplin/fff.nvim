use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fff::file_picker::FilePicker;
use fff::frecency::FrecencyTracker;
use fff::query_tracker::QueryTracker;
use fff::{FFFMode, FilePickerOptions, SharedFrecency, SharedPicker, SharedQueryTracker};
use fff_protocol::DirectoryInfoWire;

pub struct DirectoryEntry {
    pub picker: SharedPicker,
    pub frecency: SharedFrecency,
    pub query_tracker: SharedQueryTracker,
    pub last_accessed: Instant,
    pub client_pids: HashSet<u32>,
}

pub struct DaemonState {
    pub directories: HashMap<PathBuf, DirectoryEntry>,
    lru_order: VecDeque<PathBuf>,
    pub max_directories: usize,
    pub idle_timeout: Duration,
    pub frecency_db_base: Option<String>,
    pub history_db_base: Option<String>,
    pub db_unsafe_no_lock: bool,
}

impl DaemonState {
    pub fn new(max_directories: usize, idle_timeout: Duration) -> Self {
        Self {
            directories: HashMap::new(),
            lru_order: VecDeque::new(),
            max_directories,
            idle_timeout,
            frecency_db_base: None,
            history_db_base: None,
            db_unsafe_no_lock: false,
        }
    }

    pub fn init_db(
        &mut self,
        frecency_db_path: String,
        history_db_path: String,
        use_unsafe_no_lock: bool,
    ) {
        self.frecency_db_base = Some(frecency_db_path);
        self.history_db_base = Some(history_db_path);
        self.db_unsafe_no_lock = use_unsafe_no_lock;
    }

    fn db_paths_for(&self, base_path: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
        let dir_hash = stable_path_hash(base_path);
        let frecency = self
            .frecency_db_base
            .as_ref()
            .map(|base| PathBuf::from(base).join(&dir_hash));
        let history = self
            .history_db_base
            .as_ref()
            .map(|base| PathBuf::from(base).join(&dir_hash));
        (frecency, history)
    }

    pub fn ensure_directory(
        &mut self,
        path: &Path,
        watch_git_events: bool,
        client_pid: Option<u32>,
    ) -> Result<&mut DirectoryEntry, String> {
        let canonical = fff::path_utils::canonicalize(path)
            .map_err(|e| format!("failed to canonicalize path: {e}"))?;

        if self.directories.contains_key(&canonical) {
            self.touch(&canonical);
            let entry = self.directories.get_mut(&canonical).unwrap();
            if let Some(pid) = client_pid {
                entry.client_pids.insert(pid);
            }
            return Ok(entry);
        }

        self.evict_if_needed();

        let shared_picker = SharedPicker::default();
        let shared_frecency = SharedFrecency::default();
        let shared_query_tracker = SharedQueryTracker::default();

        let (frecency_db, history_db) = self.db_paths_for(&canonical);

        if let Some(ref fdb) = frecency_db {
            if let Err(e) = std::fs::create_dir_all(fdb) {
                tracing::warn!("failed to create frecency db dir: {e}");
            }
            match FrecencyTracker::new(fdb, self.db_unsafe_no_lock) {
                Ok(ft) => {
                    let _ = shared_frecency.init(ft);
                }
                Err(e) => tracing::warn!("failed to init frecency db: {e}"),
            }
        }

        if let Some(ref hdb) = history_db {
            if let Err(e) = std::fs::create_dir_all(hdb) {
                tracing::warn!("failed to create history db dir: {e}");
            }
            match QueryTracker::new(hdb, self.db_unsafe_no_lock) {
                Ok(qt) => {
                    let _ = shared_query_tracker.init(qt);
                }
                Err(e) => tracing::warn!("failed to init query tracker db: {e}"),
            }
        }

        FilePicker::new_with_shared_state(
            shared_picker.clone(),
            shared_frecency.clone(),
            FilePickerOptions {
                base_path: canonical.to_string_lossy().into_owned(),
                enable_mmap_cache: true,
                enable_content_indexing: true,
                mode: FFFMode::Neovim,
                watch: true,
                watch_git_events,
                ..Default::default()
            },
        )
        .map_err(|e| format!("failed to init file picker: {e}"))?;

        let mut client_pids = HashSet::new();
        if let Some(pid) = client_pid {
            client_pids.insert(pid);
        }

        let entry = DirectoryEntry {
            picker: shared_picker,
            frecency: shared_frecency,
            query_tracker: shared_query_tracker,
            last_accessed: Instant::now(),
            client_pids,
        };

        self.directories.insert(canonical.clone(), entry);
        self.lru_order.push_back(canonical.clone());

        Ok(self.directories.get_mut(&canonical).unwrap())
    }

    pub fn get_directory(&mut self, path: &Path) -> Option<&mut DirectoryEntry> {
        let canonical = fff::path_utils::canonicalize(path).ok()?;
        if self.directories.contains_key(&canonical) {
            self.touch(&canonical);
            self.directories.get_mut(&canonical)
        } else {
            None
        }
    }

    pub fn get_handles(
        &self,
        path: &Path,
    ) -> Result<(SharedPicker, SharedFrecency, SharedQueryTracker), String> {
        let canonical = fff::path_utils::canonicalize(path)
            .map_err(|e| format!("failed to canonicalize path: {e}"))?;
        let entry = self
            .directories
            .get(&canonical)
            .ok_or_else(|| format!("directory not indexed: {}", path.display()))?;
        Ok((
            entry.picker.clone(),
            entry.frecency.clone(),
            entry.query_tracker.clone(),
        ))
    }

    pub fn release_client(&mut self, path: &Path, client_pid: Option<u32>) -> bool {
        let canonical = match fff::path_utils::canonicalize(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(entry) = self.directories.get_mut(&canonical) {
            if let Some(pid) = client_pid {
                entry.client_pids.remove(&pid);
            }
            true
        } else {
            false
        }
    }

    pub fn drop_directory(&mut self, path: &Path) -> bool {
        let canonical = match fff::path_utils::canonicalize(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        if self.directories.remove(&canonical).is_some() {
            self.lru_order.retain(|p| p != &canonical);
            true
        } else {
            false
        }
    }

    pub fn list_directories(&self) -> Vec<DirectoryInfoWire> {
        self.directories
            .iter()
            .map(|(path, entry)| {
                let (is_scanning, file_count) = entry
                    .picker
                    .read()
                    .ok()
                    .and_then(|guard| {
                        guard.as_ref().map(|p| {
                            let progress = p.get_scan_progress();
                            (progress.is_scanning, progress.scanned_files_count)
                        })
                    })
                    .unwrap_or((false, 0));
                let mut client_pids: Vec<u32> = entry.client_pids.iter().copied().collect();
                client_pids.sort_unstable();
                DirectoryInfoWire {
                    path: path.clone(),
                    is_scanning,
                    file_count,
                    client_count: client_pids.len(),
                    client_pids,
                }
            })
            .collect()
    }

    pub fn evict_idle(&mut self) {
        let now = Instant::now();
        let idle_timeout = self.idle_timeout;
        let to_evict: Vec<PathBuf> = self
            .directories
            .iter()
            .filter(|(_, entry)| {
                entry.client_pids.is_empty()
                    && now.duration_since(entry.last_accessed) > idle_timeout
            })
            .map(|(path, _)| path.clone())
            .collect();

        for path in to_evict {
            tracing::info!("evicting idle directory: {}", path.display());
            self.directories.remove(&path);
            self.lru_order.retain(|p| p != &path);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.directories.is_empty()
    }

    fn touch(&mut self, path: &PathBuf) {
        if let Some(entry) = self.directories.get_mut(path) {
            entry.last_accessed = Instant::now();
        }
        self.lru_order.retain(|p| p != path);
        self.lru_order.push_back(path.clone());
    }

    fn evict_if_needed(&mut self) {
        let max_attempts = self.lru_order.len();
        let mut attempts = 0;
        while self.directories.len() >= self.max_directories {
            if attempts >= max_attempts {
                tracing::warn!(
                    "all {} directories have active clients, cannot evict",
                    self.directories.len()
                );
                break;
            }
            let Some(oldest) = self.lru_order.pop_front() else {
                break;
            };
            if let Some(entry) = self.directories.get(&oldest) {
                if !entry.client_pids.is_empty() {
                    self.lru_order.push_back(oldest);
                    attempts += 1;
                    continue;
                }
            }
            tracing::info!("evicting LRU directory: {}", oldest.display());
            self.directories.remove(&oldest);
        }
    }
}

fn stable_path_hash(path: &Path) -> String {
    // FNV-1a 64-bit: deterministic across Rust versions and platforms
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
