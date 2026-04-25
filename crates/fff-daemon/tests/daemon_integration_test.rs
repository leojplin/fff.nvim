use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use fff_protocol::*;

struct TestDaemon {
    child: Child,
    socket_path: PathBuf,
    _tmpdir: tempfile::TempDir,
}

impl TestDaemon {
    fn start() -> Self {
        let tmpdir = tempfile::tempdir().expect("failed to create tmpdir");
        let socket_path = tmpdir.path().join("test.sock");

        let daemon_bin = Self::find_daemon_binary();

        let child = Command::new(&daemon_bin)
            .arg("--foreground")
            .arg("--socket")
            .arg(&socket_path)
            .arg("--auto-shutdown")
            .arg("60")
            .arg("--log-level")
            .arg("debug")
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", daemon_bin.display()));

        // Wait for socket to appear
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(socket_path.exists(), "daemon did not create socket");

        Self {
            child,
            socket_path,
            _tmpdir: tmpdir,
        }
    }

    fn connect(&self) -> TestClient {
        let stream =
            UnixStream::connect(&self.socket_path).expect("failed to connect to daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let writer = BufWriter::new(stream);
        TestClient { reader, writer }
    }

    fn find_daemon_binary() -> PathBuf {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = PathBuf::from(manifest_dir).join("../..").canonicalize().unwrap();

        for profile in ["debug", "release"] {
            let candidate = workspace_root.join("target").join(profile).join("fff-daemon");
            if candidate.exists() {
                return candidate;
            }
        }

        panic!("fff-daemon binary not found — run `cargo build -p fff-daemon` first");
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestClient {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

impl TestClient {
    fn send(&mut self, request: &Request) -> Response {
        let data = encode_request(request).expect("encode failed");
        write_frame(&mut self.writer, &data).expect("write failed");
        let resp_data = read_frame(&mut self.reader).expect("read failed");
        decode_response(&resp_data).expect("decode failed")
    }
}

fn create_test_files(dir: &Path) {
    std::fs::write(dir.join("hello.txt"), "hello world\n").unwrap();
    std::fs::write(dir.join("foo.rs"), "fn main() { println!(\"foo\"); }\n").unwrap();
    std::fs::write(dir.join("bar.rs"), "fn bar() { println!(\"bar\"); }\n").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/nested.txt"), "nested content\n").unwrap();
}

#[test]
fn test_ping_pong() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let resp = client.send(&Request::Ping);
    assert!(matches!(resp, Response::Pong));
}

#[test]
fn test_index_directory_and_search() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    // Index the directory
    let resp = client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    assert!(
        matches!(resp, Response::Bool(true)),
        "IndexDirectory failed: {resp:?}"
    );

    // Wait for scan to complete
    let resp = client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });
    assert!(
        matches!(resp, Response::Bool(true)),
        "WaitForScan failed: {resp:?}"
    );

    // Verify scan progress
    let resp = client.send(&Request::GetScanProgress {
        path: test_dir.path().to_path_buf(),
    });
    match &resp {
        Response::ScanProgress(p) => {
            assert!(!p.is_scanning);
            assert!(p.scanned_files_count >= 4, "expected >= 4 files, got {}", p.scanned_files_count);
        }
        other => panic!("expected ScanProgress, got {other:?}"),
    }

    // Fuzzy search for "foo"
    let resp = client.send(&Request::FuzzySearch {
        path: test_dir.path().to_path_buf(),
        query: "foo".to_string(),
        max_threads: 1,
        current_file: None,
        combo_boost_score_multiplier: 0,
        min_combo_count: 3,
        page_index: 0,
        page_size: 10,
    });
    match &resp {
        Response::SearchResult(r) => {
            assert!(r.total_matched > 0, "expected matches for 'foo'");
            let paths: Vec<&str> = r.items.iter().map(|i| i.relative_path.as_str()).collect();
            assert!(
                paths.iter().any(|p| p.contains("foo.rs")),
                "expected foo.rs in results: {paths:?}"
            );
        }
        other => panic!("expected SearchResult, got {other:?}"),
    }

    // Fuzzy search for "nested"
    let resp = client.send(&Request::FuzzySearch {
        path: test_dir.path().to_path_buf(),
        query: "nested".to_string(),
        max_threads: 1,
        current_file: None,
        combo_boost_score_multiplier: 0,
        min_combo_count: 3,
        page_index: 0,
        page_size: 10,
    });
    match &resp {
        Response::SearchResult(r) => {
            assert!(r.total_matched > 0, "expected matches for 'nested'");
            let paths: Vec<&str> = r.items.iter().map(|i| i.relative_path.as_str()).collect();
            assert!(
                paths.iter().any(|p| p.contains("nested.txt")),
                "expected nested.txt in results: {paths:?}"
            );
        }
        other => panic!("expected SearchResult, got {other:?}"),
    }
}

#[test]
fn test_grep_search() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    // Grep for "println"
    let resp = client.send(&Request::GrepSearch {
        path: test_dir.path().to_path_buf(),
        query: "println".to_string(),
        file_offset: 0,
        page_size: 50,
        max_file_size: 10 * 1024 * 1024,
        max_matches_per_file: 200,
        smart_case: true,
        grep_mode: GrepModeWire::PlainText,
        time_budget_ms: 0,
        trim_whitespace: false,
    });
    match &resp {
        Response::GrepResult(r) => {
            assert!(r.total_matched >= 2, "expected >= 2 matches for 'println', got {}", r.total_matched);
            let files: Vec<&str> = r.items.iter().map(|i| i.relative_path.as_str()).collect();
            assert!(
                files.iter().any(|f| f.contains("foo.rs")),
                "expected foo.rs in grep results: {files:?}"
            );
        }
        other => panic!("expected GrepResult, got {other:?}"),
    }
}

#[test]
fn test_list_directories() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    let resp = client.send(&Request::ListDirectories);
    match &resp {
        Response::Directories(dirs) => {
            assert!(!dirs.is_empty(), "expected at least 1 directory");
            assert!(dirs[0].file_count >= 4, "expected >= 4 files, got {}", dirs[0].file_count);
            assert!(dirs[0].client_count >= 1, "expected at least 1 client, got {}", dirs[0].client_count);
            assert!(
                dirs[0].client_pids.contains(&std::process::id()),
                "expected client pid list to include current process {}: {:?}",
                std::process::id(),
                dirs[0].client_pids
            );
        }
        other => panic!("expected Directories, got {other:?}"),
    }
}

#[test]
fn test_health_check() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    let resp = client.send(&Request::HealthCheck {
        path: test_dir.path().to_path_buf(),
    });
    match &resp {
        Response::HealthCheck(h) => {
            assert!(h.file_picker_initialized);
            assert!(h.file_picker_base_path.is_some());
            assert!(!h.version.is_empty());
        }
        other => panic!("expected HealthCheck, got {other:?}"),
    }
}

#[test]
fn test_health_check_unknown_directory() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let resp = client.send(&Request::HealthCheck {
        path: PathBuf::from("/nonexistent/path"),
    });
    match &resp {
        Response::HealthCheck(h) => {
            assert!(!h.file_picker_initialized);
        }
        other => panic!("expected HealthCheck, got {other:?}"),
    }
}

#[test]
fn test_multiple_clients() {
    let daemon = TestDaemon::start();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    // Client 1 indexes
    let mut client1 = daemon.connect();
    client1.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client1.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    // Client 2 searches the same directory
    let mut client2 = daemon.connect();
    let resp = client2.send(&Request::FuzzySearch {
        path: test_dir.path().to_path_buf(),
        query: "bar".to_string(),
        max_threads: 1,
        current_file: None,
        combo_boost_score_multiplier: 0,
        min_combo_count: 3,
        page_index: 0,
        page_size: 10,
    });
    match &resp {
        Response::SearchResult(r) => {
            assert!(r.total_matched > 0, "client2 should find 'bar'");
        }
        other => panic!("expected SearchResult, got {other:?}"),
    }
}

#[test]
fn test_cleanup_and_drop_directory() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    // Drop the directory
    let resp = client.send(&Request::DropDirectory {
        path: test_dir.path().to_path_buf(),
    });
    assert!(matches!(resp, Response::Bool(true)));

    // Verify it's gone
    let resp = client.send(&Request::ListDirectories);
    match &resp {
        Response::Directories(dirs) => {
            assert!(dirs.is_empty(), "expected 0 directories after drop");
        }
        other => panic!("expected Directories, got {other:?}"),
    }
}

#[test]
fn test_shutdown() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let resp = client.send(&Request::Shutdown);
    assert!(matches!(resp, Response::Ok));

    // Give the daemon time to exit
    std::thread::sleep(Duration::from_millis(500));

    // Connecting should now fail
    let result = UnixStream::connect(&daemon.socket_path);
    assert!(result.is_err(), "daemon should have shut down");
}

#[test]
fn test_parse_grep_query() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let resp = client.send(&Request::ParseGrepQuery {
        query: "hello world".to_string(),
    });
    match &resp {
        Response::ParsedGrepQuery { grep_text } => {
            assert!(!grep_text.is_empty());
        }
        other => panic!("expected ParsedGrepQuery, got {other:?}"),
    }
}

#[test]
fn test_destroy_db_returns_error() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let resp = client.send(&Request::DestroyFrecencyDb);
    assert!(matches!(resp, Response::Error(_)));

    let resp = client.send(&Request::DestroyQueryDb);
    assert!(matches!(resp, Response::Error(_)));
}

#[test]
fn test_is_scanning() {
    let daemon = TestDaemon::start();
    let mut client = daemon.connect();

    let test_dir = tempfile::tempdir().unwrap();
    create_test_files(test_dir.path());

    client.send(&Request::IndexDirectory {
        path: test_dir.path().to_path_buf(),
        watch_git_events: false,
    });
    client.send(&Request::WaitForScan {
        path: test_dir.path().to_path_buf(),
        timeout_ms: 5000,
    });

    let resp = client.send(&Request::IsScanning {
        path: test_dir.path().to_path_buf(),
    });
    assert!(
        matches!(resp, Response::Bool(false)),
        "expected not scanning after wait, got {resp:?}"
    );
}
