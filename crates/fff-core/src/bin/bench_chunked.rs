//! Benchmark for chunked SIMD matching.
//! Usage: cargo run --release --bin bench_chunked -- <repo_path> [query]

use fff_search::file_picker::{FFFMode, FilePicker, FilePickerOptions};
use neo_frizbee::Config;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: bench_chunked <repo_path> [query]");
        std::process::exit(1);
    }

    let repo_path = &args[1];
    let query = args.get(2).map(|s| s.as_str()).unwrap_or("controller");

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    // --- INDEX ---
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: repo_path.to_string(),
        warmup_mmap_cache: false,
        mode: FFFMode::Neovim,
        ..Default::default()
    })
    .expect("Failed to create FilePicker");
    picker.collect_files().expect("Failed to collect files");

    let files = picker.get_files();
    let dirs = picker.get_dirs();
    let arena = picker.arena_base_ptr();
    eprintln!(
        "=== {} ===",
        repo_path.rsplit('/').next().unwrap_or(repo_path)
    );
    eprintln!(
        "Files: {}, Dirs: {}, Threads: {}",
        files.len(),
        dirs.len(),
        threads
    );

    // --- MEMORY ---
    let (chunk_arena, _, _) = picker.arena_bytes();
    eprintln!("\n--- Memory ---");
    eprintln!(
        "  ChunkedPathStore:   {:.2} KB",
        chunk_arena as f64 / 1024.0
    );

    let config = Config {
        max_typos: Some(2),
        sort: true,
        ..Default::default()
    };

    let queries = if query == "multi" {
        vec![
            "controller",
            "component",
            "Button",
            "index",
            "main",
            "server",
            "util",
            "test",
        ]
    } else {
        vec![query]
    };

    // Build string paths for contiguous matching comparison
    let path_strings: Vec<String> = files.iter().map(|f| f.relative_path(arena)).collect();
    let path_refs: Vec<&str> = path_strings.iter().map(String::as_str).collect();
    // File references for resolver-based chunked matching
    let path_refs_owned: Vec<&fff_search::types::FileItem> = files.iter().collect();

    for q in &queries {
        eprintln!("\n--- Query: '{}' ---", q);

        // Warmup
        for _ in 0..5 {
            let _ = neo_frizbee::match_list_parallel(*q, &path_refs, &config, threads);
        }

        // --- CONTIGUOUS (string-based) ---
        let iterations = 200;
        let mut seg_total = std::time::Duration::ZERO;
        let mut seg_matches = 0usize;
        for _ in 0..iterations {
            let start = Instant::now();
            let matches = neo_frizbee::match_list_parallel(*q, &path_refs, &config, threads);
            seg_total += start.elapsed();
            seg_matches = matches.len();
        }
        let seg_avg = seg_total.as_secs_f64() * 1000.0 / iterations as f64;

        // --- CHUNKED (resolver-based, zero alloc) ---
        let arena_ptr = fff_search::simd_path::ArenaPtr::new(arena);
        let resolve_file = |file: &&fff_search::types::FileItem,
                            ptrs_buf: &mut [*const u8; 32]|
         -> Option<(usize, u16)> {
            if file.is_deleted() || file.path.is_empty() {
                return None;
            }
            let resolved = file.path.resolve_ptrs(arena_ptr.as_ptr(), ptrs_buf);
            Some((resolved.len(), file.path.byte_len))
        };

        // Warmup
        for _ in 0..5 {
            let _ = neo_frizbee::match_list_parallel_resolved(
                *q,
                &path_refs_owned,
                &resolve_file,
                &config,
                threads,
            );
        }

        let mut chunk_total = std::time::Duration::ZERO;
        let mut chunk_matches = 0usize;
        for _ in 0..iterations {
            let start = Instant::now();
            let matches = neo_frizbee::match_list_parallel_resolved(
                *q,
                &path_refs_owned,
                &resolve_file,
                &config,
                threads,
            );
            chunk_total += start.elapsed();
            chunk_matches = matches.len();
        }
        let chunk_avg = chunk_total.as_secs_f64() * 1000.0 / iterations as f64;

        // --- RESULTS ---
        let speedup = seg_avg / chunk_avg;
        eprintln!(
            "  Contiguous: {:.3}ms avg ({} matches)",
            seg_avg, seg_matches
        );
        eprintln!(
            "  Chunked:    {:.3}ms avg ({} matches)",
            chunk_avg, chunk_matches
        );
        eprintln!(
            "  Speedup:    {:.2}x ({})",
            speedup,
            if speedup > 1.0 {
                "chunked faster"
            } else {
                "contiguous faster"
            }
        );

        // Verify score parity on first match
        if seg_matches > 0 && chunk_matches > 0 {
            let seg_result = neo_frizbee::match_list_parallel(*q, &path_refs, &config, threads);
            let chunk_result = neo_frizbee::match_list_parallel_resolved(
                *q,
                &path_refs_owned,
                &resolve_file,
                &config,
                threads,
            );

            let seg_top = &seg_result[0];
            let chunk_top = &chunk_result[0];
            if seg_top.score == chunk_top.score && seg_top.index == chunk_top.index {
                eprintln!(
                    "  Parity:     OK (top match: idx={}, score={})",
                    seg_top.index, seg_top.score
                );
            } else {
                eprintln!(
                    "  Parity:     MISMATCH! seg=(idx={}, score={}) chunk=(idx={}, score={})",
                    seg_top.index, seg_top.score, chunk_top.index, chunk_top.score
                );
            }
        }
    }
}
