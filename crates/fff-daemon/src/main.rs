mod handler;
mod state;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use fff_protocol::{
    decode_request, encode_response, socket_path,
    Request, Response,
};
use mimalloc::MiMalloc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use crate::state::DaemonState;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser)]
#[command(name = "fff-daemon", about = "FFF file search daemon")]
struct Args {
    #[arg(long, help = "Socket path (default: auto-detected)")]
    socket: Option<String>,

    #[arg(long, default_value = "20", help = "Max indexed directories")]
    max_directories: usize,

    #[arg(
        long,
        default_value = "1800",
        help = "Idle timeout in seconds before evicting unused directories"
    )]
    idle_timeout: u64,

    #[arg(
        long,
        default_value = "3600",
        help = "Auto-shutdown after N seconds with no clients (0 = never)"
    )]
    auto_shutdown: u64,

    #[arg(long, default_value = "info", help = "Log level")]
    log_level: String,

    #[arg(long, help = "Log to file instead of stderr")]
    log_file: Option<String>,

    #[arg(long, help = "Run in foreground (don't daemonize)")]
    foreground: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Set up logging
    let filter = EnvFilter::try_new(&args.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    if let Some(ref log_file) = args.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .init();
    }

    let sock_path = args
        .socket
        .map(std::path::PathBuf::from)
        .unwrap_or_else(socket_path);

    // Ensure socket directory exists with restricted permissions
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    let pid_path = sock_path.with_extension("pid");

    // Clean up stale socket
    if sock_path.exists() {
        if pid_path.exists() {
            if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    // SAFETY: kill(pid, 0) only checks process existence, no signal sent
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    if alive {
                        eprintln!("fff-daemon already running (pid {pid})");
                        std::process::exit(1);
                    }
                }
            }
        }
        std::fs::remove_file(&sock_path)?;
    }

    std::fs::write(&pid_path, std::process::id().to_string())?;

    let listener = UnixListener::bind(&sock_path)?;
    tracing::info!("listening on {}", sock_path.display());

    let state = Arc::new(RwLock::new(DaemonState::new(
        args.max_directories,
        Duration::from_secs(args.idle_timeout),
    )));

    let auto_shutdown_secs = args.auto_shutdown;
    let shutdown_state = state.clone();

    // Idle eviction + auto-shutdown task
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let shutdown_notify_clone = shutdown_notify.clone();

    tokio::spawn(async move {
        let mut last_activity = tokio::time::Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            {
                let mut s = shutdown_state.write().await;
                s.evict_idle();
                if !s.is_empty() {
                    last_activity = tokio::time::Instant::now();
                }
            }
            if auto_shutdown_secs > 0 {
                let idle = last_activity.elapsed();
                if idle > Duration::from_secs(auto_shutdown_secs) {
                    tracing::info!("auto-shutting down after {}s idle", idle.as_secs());
                    shutdown_notify_clone.notify_one();
                    break;
                }
            }
        }
    });

    // Accept loop
    let accept_state = state.clone();
    let accept_shutdown = shutdown_notify.clone();

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let conn_state = accept_state.clone();
                        let conn_shutdown = accept_shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, conn_state, conn_shutdown).await {
                                tracing::debug!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                    }
                }
            }
            _ = shutdown_notify.notified() => {
                tracing::info!("shutting down");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c received, shutting down");
                break;
            }
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: Arc<RwLock<DaemonState>>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client_pid = stream
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .map(|pid| pid as u32);
    let mut registered_paths = HashSet::<PathBuf>::new();
    let (mut reader, mut writer) = stream.into_split();

    loop {
        // Read frame length
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 64 * 1024 * 1024 {
            let resp = encode_response(&Response::Error("frame too large".into()))?;
            writer.write_all(&(resp.len() as u32).to_be_bytes()).await?;
            writer.write_all(&resp).await?;
            writer.flush().await?;
            continue;
        }

        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;

        let request = match decode_request(&buf) {
            Ok(r) => r,
            Err(e) => {
                let resp =
                    encode_response(&Response::Error(format!("decode error: {e}")))?;
                writer.write_all(&(resp.len() as u32).to_be_bytes()).await?;
                writer.write_all(&resp).await?;
                writer.flush().await?;
                continue;
            }
        };

        let is_shutdown = matches!(request, Request::Shutdown);
        let request_for_tracking = request.clone();

        let response = handler::handle_request(state.clone(), request, client_pid).await;

        match (&request_for_tracking, &response) {
            (Request::IndexDirectory { path, .. }, Response::Bool(true)) => {
                registered_paths.insert(canonicalize_or_original(path));
            }
            (Request::RestartIndex { path, new_path }, Response::Ok) => {
                registered_paths.remove(&canonicalize_or_original(path));
                registered_paths.insert(canonicalize_or_original(new_path));
            }
            (Request::CleanupFilePicker { path }, Response::Bool(true))
            | (Request::DropDirectory { path }, Response::Bool(true)) => {
                registered_paths.remove(&canonicalize_or_original(path));
            }
            _ => {}
        }

        let resp_bytes = encode_response(&response)?;
        writer.write_all(&(resp_bytes.len() as u32).to_be_bytes()).await?;
        writer.write_all(&resp_bytes).await?;
        writer.flush().await?;

        if is_shutdown {
            shutdown_notify.notify_one();
            break;
        }
    }

    if !registered_paths.is_empty() {
        let mut guard = state.write().await;
        for path in registered_paths {
            let _ = guard.release_client(&path, client_pid);
        }
    }

    Ok(())
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    fff::path_utils::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
