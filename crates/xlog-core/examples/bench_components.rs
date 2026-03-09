use std::env;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use filetime::{set_file_mtime, FileTime};
use mars_xlog_core::compress::{
    decompress_raw_zlib, decompress_zstd_frames, StreamCompressor, ZlibStreamCompressor,
    ZstdChunkCompressor, ZstdStreamCompressor,
};
use mars_xlog_core::crypto::{tea_encrypt_in_place, EcdhTeaCipher};
use mars_xlog_core::file_manager::FileManager;
use mars_xlog_core::formatter::{format_record, format_record_parts_into};
use mars_xlog_core::record::{LogLevel, LogRecord};
use tempfile::TempDir;

const SAMPLE_PUBKEY: &str =
    "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8";

const USAGE: &str = "\
Micro-benchmark xlog core components.

Usage:
  cargo run -p mars-xlog-core --example bench_components -- [target] [options]

Targets:
  all | compress | crypto | formatter | io

Options:
  --iterations <n>   Iterations per benchmark (default: 100000)
  --payload-size <n> Payload size in bytes (default: 256)
  --io-iterations <n> Iterations for file I/O path benchmarks (default: min(iterations, 2000))
";

#[derive(Copy, Clone)]
enum Target {
    All,
    Compress,
    Crypto,
    Formatter,
    Io,
}

impl Target {
    fn parse(input: &str) -> Result<Self, String> {
        match input {
            "all" => Ok(Target::All),
            "compress" => Ok(Target::Compress),
            "crypto" => Ok(Target::Crypto),
            "formatter" => Ok(Target::Formatter),
            "io" => Ok(Target::Io),
            _ => Err(format!("invalid target: {input}")),
        }
    }
}

struct Options {
    target: Target,
    iterations: usize,
    payload_size: usize,
    io_iterations: usize,
}

#[derive(Copy, Clone)]
struct ProcIoSnapshot {
    read_syscalls: u64,
    write_syscalls: u64,
    read_bytes: u64,
    write_bytes: u64,
}

#[derive(Copy, Clone)]
struct ResourceSnapshot {
    user_us: i64,
    sys_us: i64,
    max_rss_kb: i64,
    proc_io: Option<ProcIoSnapshot>,
}

#[derive(Copy, Clone, Default)]
struct ResourceDelta {
    cpu_user_ms: Option<f64>,
    cpu_system_ms: Option<f64>,
    max_rss_kb: Option<i64>,
    io_read_syscalls: Option<u64>,
    io_write_syscalls: Option<u64>,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
}

#[derive(Copy, Clone, Default)]
struct IoEventDelta {
    scanned_entries: Option<u64>,
    moved_files: Option<u64>,
    deleted_files: Option<u64>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    match opts.target {
        Target::All => {
            run_compress(&opts)?;
            run_crypto(&opts)?;
            run_formatter(&opts)?;
            run_io(&opts)?;
        }
        Target::Compress => run_compress(&opts)?,
        Target::Crypto => run_crypto(&opts)?,
        Target::Formatter => run_formatter(&opts)?,
        Target::Io => run_io(&opts)?,
    }
    Ok(())
}

fn parse_args() -> Result<Options, String> {
    let mut target = Target::All;
    let mut iterations = 100_000usize;
    let mut payload_size = 256usize;
    let mut io_iterations: Option<usize> = None;
    let mut target_set = false;

    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            "--iterations" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--iterations requires a value".to_string())?;
                iterations = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --iterations value {v}: {e}"))?;
            }
            "--payload-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--payload-size requires a value".to_string())?;
                payload_size = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --payload-size value {v}: {e}"))?;
            }
            "--io-iterations" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--io-iterations requires a value".to_string())?;
                io_iterations = Some(
                    v.parse::<usize>()
                        .map_err(|e| format!("invalid --io-iterations value {v}: {e}"))?,
                );
            }
            token if token.starts_with("--") => {
                return Err(format!("unknown argument: {token}\n\n{USAGE}"));
            }
            token => {
                if target_set {
                    return Err(format!(
                        "unexpected positional argument: {token}\n\n{USAGE}"
                    ));
                }
                target = Target::parse(token)?;
                target_set = true;
            }
        }
    }

    if iterations == 0 {
        return Err("--iterations must be > 0".to_string());
    }
    if payload_size == 0 {
        return Err("--payload-size must be > 0".to_string());
    }
    let io_iterations = io_iterations.unwrap_or(iterations.min(2_000)).max(1);

    Ok(Options {
        target,
        iterations,
        payload_size,
        io_iterations,
    })
}

fn run_compress(opts: &Options) -> Result<(), String> {
    let payload = make_payload(opts.payload_size, 0xA5A5_A5A5_1234_5678);

    bench_stream_compressor(
        "zlib_stream_l6",
        opts,
        payload.as_slice(),
        ZlibStreamCompressor::new(6),
        decompress_raw_zlib,
    )?;
    bench_stream_compressor(
        "zlib_stream_l9",
        opts,
        payload.as_slice(),
        ZlibStreamCompressor::new(9),
        decompress_raw_zlib,
    )?;
    bench_stream_compressor(
        "zstd_stream_l3",
        opts,
        payload.as_slice(),
        ZstdStreamCompressor::new(3).map_err(|e| format!("zstd stream init: {e}"))?,
        decompress_zstd_frames,
    )?;
    bench_stream_compressor(
        "zstd_chunk_l3",
        opts,
        payload.as_slice(),
        ZstdChunkCompressor::new(3),
        decompress_zstd_frames,
    )?;
    Ok(())
}

fn run_crypto(opts: &Options) -> Result<(), String> {
    let cipher = EcdhTeaCipher::new_with_private_key(SAMPLE_PUBKEY, [7u8; 32])
        .map_err(|e| format!("build cipher failed: {e}"))?;
    let key = cipher.tea_key_words();
    let aligned = (opts.payload_size / 8).max(1) * 8;
    let mut block = make_payload(aligned, 0x1234_5678_9ABC_DEF0);
    let input_bytes = aligned.saturating_mul(opts.iterations);

    let (start, start_res) = begin_measurement();
    for _ in 0..opts.iterations {
        tea_encrypt_in_place(black_box(block.as_mut_slice()), black_box(&key));
    }
    let (elapsed_ms, resources) = end_measurement(start, start_res);
    emit_result(
        "crypto",
        "tea_encrypt",
        opts.payload_size,
        opts.iterations,
        elapsed_ms,
        input_bytes,
        input_bytes,
        1.0,
        resources,
        IoEventDelta::default(),
    );

    let (start, start_res) = begin_measurement();
    let mut sink = 0u64;
    for idx in 0..opts.iterations {
        let mut private_key = [0u8; 32];
        private_key[0] = 1;
        private_key[30] = ((idx >> 8) & 0xFF) as u8;
        private_key[31] = (idx & 0xFF) as u8;
        if private_key[30] == 0 && private_key[31] == 0 {
            private_key[31] = 1;
        }
        let derived = EcdhTeaCipher::new_with_private_key(SAMPLE_PUBKEY, private_key)
            .map_err(|e| format!("ecdh derive failed at iteration {idx}: {e}"))?;
        sink = sink.wrapping_add(derived.client_pubkey()[0] as u64);
    }
    black_box(sink);
    let (elapsed_ms, resources) = end_measurement(start, start_res);
    let input_bytes = 32usize.saturating_mul(opts.iterations);
    emit_result(
        "crypto",
        "ecdh_derive",
        32,
        opts.iterations,
        elapsed_ms,
        input_bytes,
        input_bytes,
        1.0,
        resources,
        IoEventDelta::default(),
    );

    Ok(())
}

fn run_formatter(opts: &Options) -> Result<(), String> {
    let payload = String::from_utf8(make_payload(opts.payload_size, 0xCC55_3300_FF00_AA11))
        .map_err(|e| format!("build formatter payload failed: {e}"))?;

    let record = LogRecord {
        level: LogLevel::Info,
        tag: "bench-tag".to_string(),
        filename: "/path/to/bench_backend.rs".to_string(),
        func_name: "bench::emit".to_string(),
        line: 42,
        timestamp: std::time::SystemTime::now(),
        pid: 1001,
        tid: 1002,
        maintid: 1001,
    };

    let mut sink = 0usize;
    let (start, start_res) = begin_measurement();
    for _ in 0..opts.iterations {
        let line = format_record(black_box(&record), black_box(&payload));
        sink = sink.wrapping_add(line.len());
        black_box(&line);
    }
    black_box(sink);
    let (elapsed_ms, resources) = end_measurement(start, start_res);
    let input_bytes = opts.payload_size.saturating_mul(opts.iterations);
    emit_result(
        "formatter",
        "format_record_alloc",
        opts.payload_size,
        opts.iterations,
        elapsed_ms,
        input_bytes,
        sink,
        if input_bytes == 0 {
            0.0
        } else {
            sink as f64 / input_bytes as f64
        },
        resources,
        IoEventDelta::default(),
    );

    let mut out = String::with_capacity(16 * 1024);
    let mut sink_reuse = 0usize;
    let (start, start_res) = begin_measurement();
    for _ in 0..opts.iterations {
        format_record_parts_into(
            &mut out,
            LogLevel::Info,
            "bench-tag",
            "/path/to/bench_backend.rs",
            "bench::emit",
            42,
            std::time::SystemTime::now(),
            1001,
            1002,
            1001,
            &payload,
        );
        sink_reuse = sink_reuse.wrapping_add(out.len());
        black_box(&out);
    }
    black_box(sink_reuse);
    let (elapsed_ms, resources) = end_measurement(start, start_res);
    emit_result(
        "formatter",
        "format_record_parts_into",
        opts.payload_size,
        opts.iterations,
        elapsed_ms,
        input_bytes,
        sink_reuse,
        if input_bytes == 0 {
            0.0
        } else {
            sink_reuse as f64 / input_bytes as f64
        },
        resources,
        IoEventDelta::default(),
    );

    Ok(())
}

fn run_io(opts: &Options) -> Result<(), String> {
    let payload = make_payload(opts.payload_size.max(64), 0xF0F0_AA55_1234_6789);
    let rotate_max_file_size = (opts.payload_size.max(64) as u64).saturating_mul(2);
    let append_max_file_size = (opts.payload_size.max(64) as u64).saturating_mul(1024 * 1024);

    bench_append_path_variant(
        "append_keep_open",
        opts,
        payload.as_slice(),
        false,
        0,
        append_max_file_size,
        false,
        true,
    )?;
    bench_append_path_variant(
        "append_close_after_write",
        opts,
        payload.as_slice(),
        false,
        0,
        append_max_file_size,
        false,
        false,
    )?;
    bench_append_path_variant(
        "append_rotate_keep_open",
        opts,
        payload.as_slice(),
        false,
        0,
        rotate_max_file_size,
        false,
        true,
    )?;
    bench_append_path_variant(
        "append_rotate_close_after_write",
        opts,
        payload.as_slice(),
        false,
        0,
        rotate_max_file_size,
        false,
        false,
    )?;
    bench_append_path_variant(
        "append_cache_keep_open",
        opts,
        payload.as_slice(),
        true,
        1,
        append_max_file_size,
        false,
        true,
    )?;
    bench_flush_append_only_variant(opts, payload.as_slice())?;
    bench_flush_sweep_only_variant(opts, payload.as_slice())?;
    bench_flush_via_delete_expired_variant(opts, payload.as_slice())?;
    bench_move_old_cache_files_only_variant(opts, payload.as_slice())?;
    bench_move_old_cache_files_variant(opts, payload.as_slice())?;
    bench_delete_expired_scan_only_variant(opts, payload.as_slice())?;
    bench_delete_expired_files_variant(opts, payload.as_slice())?;
    Ok(())
}

fn bench_stream_compressor<C, D>(
    variant: &str,
    opts: &Options,
    payload: &[u8],
    mut compressor: C,
    decode: D,
) -> Result<(), String>
where
    C: StreamCompressor,
    D: Fn(&[u8]) -> Result<Vec<u8>, mars_xlog_core::compress::CompressError>,
{
    let input_bytes = payload.len().saturating_mul(opts.iterations);
    let mut compressed = Vec::with_capacity(input_bytes / 2 + 1);
    let (start, start_res) = begin_measurement();
    for _ in 0..opts.iterations {
        compressor
            .compress_chunk(black_box(payload), &mut compressed)
            .map_err(|e| format!("{variant} compress chunk failed: {e}"))?;
    }
    compressor
        .flush(&mut compressed)
        .map_err(|e| format!("{variant} flush failed: {e}"))?;
    let (elapsed_ms, resources) = end_measurement(start, start_res);
    black_box(&compressed);

    // Light decode sanity-check to avoid benchmarking broken output.
    let decoded = decode(&compressed).map_err(|e| format!("{variant} decode failed: {e}"))?;
    if decoded.len() != input_bytes {
        return Err(format!(
            "{variant} decode size mismatch: expect {input_bytes}, got {}",
            decoded.len()
        ));
    }

    let output_bytes = compressed.len();
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "compress",
        variant,
        opts.payload_size,
        opts.iterations,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta::default(),
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn bench_append_path_variant(
    variant: &str,
    opts: &Options,
    payload: &[u8],
    use_cache_dir: bool,
    cache_days: i32,
    max_file_size: u64,
    move_file: bool,
    keep_open: bool,
) -> Result<(), String> {
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = use_cache_dir.then(|| root.path().join("cache"));
    let manager = FileManager::new(
        log_dir.clone(),
        cache_dir.clone(),
        "bench".to_string(),
        cache_days,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let (start, start_res) = begin_measurement();
    for _ in 0..opts.io_iterations {
        manager
            .append_log_bytes(black_box(payload), max_file_size, move_file, keep_open)
            .map_err(|e| format!("{variant} append failed: {e}"))?;
    }
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let input_bytes = payload.len().saturating_mul(opts.io_iterations);
    let mut output_bytes = total_size_under(&log_dir);
    if let Some(cache) = cache_dir.as_ref() {
        output_bytes = output_bytes.saturating_add(total_size_under(cache));
    }
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "io",
        variant,
        opts.payload_size,
        opts.io_iterations,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta::default(),
    );
    Ok(())
}

fn bench_move_old_cache_files_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "move_old_cache_files";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        0,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let rounds = opts.io_iterations.clamp(1, 1_000);
    let mut input_bytes = 0usize;
    let (start, start_res) = begin_measurement();
    for idx in 0..rounds {
        let path = cache_dir.join(format!("bench-move-{idx}.xlog"));
        fs::write(&path, payload).map_err(|e| format!("{variant} seed file write failed: {e}"))?;
        input_bytes = input_bytes.saturating_add(payload.len());
        manager
            .move_old_cache_files(0)
            .map_err(|e| format!("{variant} move failed: {e}"))?;
    }
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "io",
        variant,
        opts.payload_size,
        rounds,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some(rounds as u64),
            moved_files: Some(rounds as u64),
            deleted_files: None,
        },
    );
    Ok(())
}

fn bench_move_old_cache_files_only_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "move_old_cache_files_only";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        0,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let seed_files = opts.io_iterations.clamp(1, 2_000);
    for idx in 0..seed_files {
        let path = cache_dir.join(format!("bench-move-only-{idx}.xlog"));
        create_old_file(&path, payload, variant)?;
    }
    let input_bytes = payload.len().saturating_mul(seed_files);
    let before_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));

    let (start, start_res) = begin_measurement();
    manager
        .move_old_cache_files(0)
        .map_err(|e| format!("{variant} move failed: {e}"))?;
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    let after_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));
    let moved_files = count_xlog_files_under(&log_dir) as u64;
    let deleted_files = before_total.saturating_sub(after_total) as u64;
    emit_result(
        "io",
        variant,
        opts.payload_size,
        seed_files,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some(before_total as u64),
            moved_files: Some(moved_files),
            deleted_files: Some(deleted_files),
        },
    );
    Ok(())
}

fn bench_flush_append_only_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "flush_append_only";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        1,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let rounds = opts.io_iterations.clamp(1, 2_000);
    let max_file_size = (payload.len().max(64) as u64).saturating_mul(1024 * 1024);
    let (start, start_res) = begin_measurement();
    for _ in 0..rounds {
        manager
            .append_log_bytes(payload, max_file_size, false, true)
            .map_err(|e| format!("{variant} append failed: {e}"))?;
    }
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    // Flush keep-open writer once outside the timed path.
    manager
        .delete_expired_files(365 * 24 * 60 * 60)
        .map_err(|e| format!("{variant} post flush failed: {e}"))?;

    let input_bytes = payload.len().saturating_mul(rounds);
    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "io",
        variant,
        opts.payload_size,
        rounds,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta::default(),
    );
    Ok(())
}

fn bench_flush_sweep_only_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "flush_sweep_only";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        1,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let rounds = opts.io_iterations.clamp(1, 1_000);
    let max_file_size = (payload.len().max(64) as u64).saturating_mul(1024 * 1024);
    for _ in 0..rounds {
        manager
            .append_log_bytes(payload, max_file_size, false, true)
            .map_err(|e| format!("{variant} seed append failed: {e}"))?;
    }
    let file_scan_entries =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));

    let (start, start_res) = begin_measurement();
    manager
        .delete_expired_files(365 * 24 * 60 * 60)
        .map_err(|e| format!("{variant} flush sweep failed: {e}"))?;
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let input_bytes = payload.len().saturating_mul(rounds);
    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "io",
        variant,
        opts.payload_size,
        1,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some(file_scan_entries as u64),
            moved_files: None,
            deleted_files: None,
        },
    );
    Ok(())
}

fn bench_flush_via_delete_expired_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "flush_via_delete_expired";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        1,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let rounds = opts.io_iterations.clamp(1, 1_000);
    let max_file_size = (payload.len().max(64) as u64).saturating_mul(1024 * 1024);
    let (start, start_res) = begin_measurement();
    for _ in 0..rounds {
        manager
            .append_log_bytes(payload, max_file_size, false, true)
            .map_err(|e| format!("{variant} append failed: {e}"))?;
        manager
            .delete_expired_files(365 * 24 * 60 * 60)
            .map_err(|e| format!("{variant} flush route failed: {e}"))?;
    }
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let input_bytes = payload.len().saturating_mul(rounds);
    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    emit_result(
        "io",
        variant,
        opts.payload_size,
        rounds,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some((rounds * 2) as u64),
            moved_files: None,
            deleted_files: None,
        },
    );
    Ok(())
}

fn bench_delete_expired_scan_only_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "delete_expired_scan_only";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        1,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let seed_files = opts.io_iterations.clamp(1, 2_000);
    for idx in 0..seed_files {
        let log_path = log_dir.join(format!("bench-fresh-log-{idx}.xlog"));
        let cache_path = cache_dir.join(format!("bench-fresh-cache-{idx}.xlog"));
        fs::write(&log_path, payload)
            .map_err(|e| format!("{variant} seed log write failed: {e}"))?;
        fs::write(&cache_path, payload)
            .map_err(|e| format!("{variant} seed cache write failed: {e}"))?;
    }
    let before_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));

    let (start, start_res) = begin_measurement();
    manager
        .delete_expired_files(365 * 24 * 60 * 60)
        .map_err(|e| format!("{variant} scan failed: {e}"))?;
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let input_bytes = payload.len().saturating_mul(seed_files.saturating_mul(2));
    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    let after_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));
    let deleted_files = before_total.saturating_sub(after_total) as u64;
    emit_result(
        "io",
        variant,
        opts.payload_size,
        seed_files.saturating_mul(2),
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some(before_total as u64),
            moved_files: None,
            deleted_files: Some(deleted_files),
        },
    );
    Ok(())
}

fn bench_delete_expired_files_variant(opts: &Options, payload: &[u8]) -> Result<(), String> {
    let variant = "delete_expired_files";
    let root = TempDir::new().map_err(|e| format!("{variant} temp dir: {e}"))?;
    let log_dir = root.path().join("logs");
    let cache_dir = root.path().join("cache");
    let manager = FileManager::new(
        log_dir.clone(),
        Some(cache_dir.clone()),
        "bench".to_string(),
        1,
    )
    .map_err(|e| format!("{variant} file manager init: {e}"))?;

    let seed_files = opts.io_iterations.clamp(1, 512);
    for idx in 0..seed_files {
        let log_path = log_dir.join(format!("bench-expired-log-{idx}.xlog"));
        let cache_path = cache_dir.join(format!("bench-expired-cache-{idx}.xlog"));
        create_old_file(&log_path, payload, variant)?;
        create_old_file(&cache_path, payload, variant)?;
    }

    // Seed one active keep-open append so delete path also covers active flush.
    let max_file_size = (payload.len().max(64) as u64).saturating_mul(1024 * 1024);
    manager
        .append_log_bytes(payload, max_file_size, false, true)
        .map_err(|e| format!("{variant} active append failed: {e}"))?;
    let before_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));

    let (start, start_res) = begin_measurement();
    manager
        .delete_expired_files(1)
        .map_err(|e| format!("{variant} delete failed: {e}"))?;
    let (elapsed_ms, resources) = end_measurement(start, start_res);

    let logical_ops = seed_files.saturating_mul(2).saturating_add(1);
    let input_bytes = payload.len().saturating_mul(logical_ops);
    let output_bytes = total_size_under(&log_dir).saturating_add(total_size_under(&cache_dir));
    let ratio = if input_bytes == 0 {
        0.0
    } else {
        output_bytes as f64 / input_bytes as f64
    };
    let after_total =
        count_xlog_files_under(&log_dir).saturating_add(count_xlog_files_under(&cache_dir));
    let deleted_files = before_total.saturating_sub(after_total) as u64;
    emit_result(
        "io",
        variant,
        opts.payload_size,
        logical_ops,
        elapsed_ms,
        input_bytes,
        output_bytes,
        ratio,
        resources,
        IoEventDelta {
            scanned_entries: Some(before_total as u64),
            moved_files: None,
            deleted_files: Some(deleted_files),
        },
    );
    Ok(())
}

fn create_old_file(path: &Path, payload: &[u8], variant: &str) -> Result<(), String> {
    fs::write(path, payload).map_err(|e| format!("{variant} seed file write failed: {e}"))?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("{variant} system time error: {e}"))?
        .as_secs() as i64;
    let old = FileTime::from_unix_time(now_secs.saturating_sub(2 * 60 * 60), 0);
    set_file_mtime(path, old).map_err(|e| format!("{variant} set mtime failed: {e}"))?;
    Ok(())
}

fn total_size_under(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return path.metadata().map(|m| m.len() as usize).unwrap_or(0);
    }

    let mut total = 0usize;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total = total.saturating_add(total_size_under(&entry.path()));
        }
    }
    total
}

fn count_xlog_files_under(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| usize::from(ext == "xlog"))
            .unwrap_or(0);
    }

    let mut total = 0usize;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total = total.saturating_add(count_xlog_files_under(&entry.path()));
        }
    }
    total
}

fn begin_measurement() -> (Instant, Option<ResourceSnapshot>) {
    (Instant::now(), capture_resource_snapshot())
}

fn end_measurement(start: Instant, start_res: Option<ResourceSnapshot>) -> (f64, ResourceDelta) {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let end_res = capture_resource_snapshot();
    let delta = match (start_res, end_res) {
        (Some(s), Some(e)) => {
            let (io_read_syscalls, io_write_syscalls, io_read_bytes, io_write_bytes) =
                match (s.proc_io, e.proc_io) {
                    (Some(start_io), Some(end_io)) => (
                        Some(end_io.read_syscalls.saturating_sub(start_io.read_syscalls)),
                        Some(
                            end_io
                                .write_syscalls
                                .saturating_sub(start_io.write_syscalls),
                        ),
                        Some(end_io.read_bytes.saturating_sub(start_io.read_bytes)),
                        Some(end_io.write_bytes.saturating_sub(start_io.write_bytes)),
                    ),
                    _ => (None, None, None, None),
                };
            ResourceDelta {
                cpu_user_ms: Some((e.user_us - s.user_us) as f64 / 1000.0),
                cpu_system_ms: Some((e.sys_us - s.sys_us) as f64 / 1000.0),
                max_rss_kb: Some(e.max_rss_kb),
                io_read_syscalls,
                io_write_syscalls,
                io_read_bytes,
                io_write_bytes,
            }
        }
        _ => ResourceDelta::default(),
    };
    (elapsed_ms, delta)
}

#[cfg(unix)]
fn micros_to_i64(value: libc::suseconds_t) -> i64 {
    #[allow(clippy::unnecessary_cast)]
    {
        value as i64
    }
}

#[cfg(unix)]
fn capture_resource_snapshot() -> Option<ResourceSnapshot> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }

    let user_us = usage
        .ru_utime
        .tv_sec
        .saturating_mul(1_000_000)
        .saturating_add(micros_to_i64(usage.ru_utime.tv_usec));
    let sys_us = usage
        .ru_stime
        .tv_sec
        .saturating_mul(1_000_000)
        .saturating_add(micros_to_i64(usage.ru_stime.tv_usec));

    let raw_max_rss = usage.ru_maxrss;
    #[cfg(target_os = "macos")]
    let max_rss_kb = raw_max_rss.saturating_div(1024);
    #[cfg(not(target_os = "macos"))]
    let max_rss_kb = raw_max_rss;

    Some(ResourceSnapshot {
        user_us,
        sys_us,
        max_rss_kb,
        proc_io: capture_proc_io_snapshot(),
    })
}

#[cfg(not(unix))]
fn capture_resource_snapshot() -> Option<ResourceSnapshot> {
    None
}

#[cfg(target_os = "linux")]
fn capture_proc_io_snapshot() -> Option<ProcIoSnapshot> {
    let content = fs::read_to_string("/proc/self/io").ok()?;
    let mut read_syscalls = None;
    let mut write_syscalls = None;
    let mut read_bytes = None;
    let mut write_bytes = None;

    for line in content.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(key) = parts.next().map(str::trim) else {
            continue;
        };
        let Some(raw_value) = parts.next().map(str::trim) else {
            continue;
        };
        let Ok(value) = raw_value.parse::<u64>() else {
            continue;
        };
        match key {
            "syscr" => read_syscalls = Some(value),
            "syscw" => write_syscalls = Some(value),
            "read_bytes" => read_bytes = Some(value),
            "write_bytes" => write_bytes = Some(value),
            _ => {}
        }
    }

    Some(ProcIoSnapshot {
        read_syscalls: read_syscalls?,
        write_syscalls: write_syscalls?,
        read_bytes: read_bytes?,
        write_bytes: write_bytes?,
    })
}

#[cfg(not(target_os = "linux"))]
fn capture_proc_io_snapshot() -> Option<ProcIoSnapshot> {
    None
}

#[allow(clippy::too_many_arguments)]
fn emit_result(
    component: &str,
    variant: &str,
    payload_size: usize,
    iterations: usize,
    elapsed_ms: f64,
    input_bytes: usize,
    output_bytes: usize,
    ratio: f64,
    resources: ResourceDelta,
    io_events: IoEventDelta,
) {
    let elapsed_s = (elapsed_ms / 1000.0).max(1e-12);
    let ops_per_sec = iterations as f64 / elapsed_s;
    let bytes_per_sec = input_bytes as f64 / elapsed_s;
    let output_bytes_per_sec = output_bytes as f64 / elapsed_s;
    let cpu_user_ms = resources
        .cpu_user_ms
        .map(|v| format!("{v:.3}"))
        .unwrap_or_else(|| "null".to_string());
    let cpu_system_ms = resources
        .cpu_system_ms
        .map(|v| format!("{v:.3}"))
        .unwrap_or_else(|| "null".to_string());
    let max_rss_kb = resources
        .max_rss_kb
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let io_read_syscalls = resources
        .io_read_syscalls
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let io_write_syscalls = resources
        .io_write_syscalls
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let io_read_bytes = resources
        .io_read_bytes
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let io_write_bytes = resources
        .io_write_bytes
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let io_write_bytes_per_sec = resources
        .io_write_bytes
        .map(|v| format!("{:.3}", v as f64 / elapsed_s))
        .unwrap_or_else(|| "null".to_string());
    let syscalls_per_op = resources
        .io_read_syscalls
        .zip(resources.io_write_syscalls)
        .map(|(r, w)| (r + w) as f64 / iterations.max(1) as f64)
        .map(|v| format!("{v:.6}"))
        .unwrap_or_else(|| "null".to_string());
    let scanned_entries = io_events
        .scanned_entries
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let moved_files = io_events
        .moved_files
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let deleted_files = io_events
        .deleted_files
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    println!(
        "{{\"component\":\"{}\",\"variant\":\"{}\",\"payload_size\":{},\"iterations\":{},\"elapsed_ms\":{:.3},\"ops_per_sec\":{:.3},\"bytes_per_sec\":{:.3},\"output_bytes_per_sec\":{:.3},\"input_bytes\":{},\"output_bytes\":{},\"ratio\":{:.6},\"cpu_user_ms\":{},\"cpu_system_ms\":{},\"max_rss_kb\":{},\"io_read_syscalls\":{},\"io_write_syscalls\":{},\"io_read_bytes\":{},\"io_write_bytes\":{},\"io_write_bytes_per_sec\":{},\"syscalls_per_op\":{},\"scanned_entries\":{},\"moved_files\":{},\"deleted_files\":{}}}",
        component,
        variant,
        payload_size,
        iterations,
        elapsed_ms,
        ops_per_sec,
        bytes_per_sec,
        output_bytes_per_sec,
        input_bytes,
        output_bytes,
        ratio,
        cpu_user_ms,
        cpu_system_ms,
        max_rss_kb,
        io_read_syscalls,
        io_write_syscalls,
        io_read_bytes,
        io_write_bytes,
        io_write_bytes_per_sec,
        syscalls_per_op,
        scanned_entries,
        moved_files,
        deleted_files
    );
}

fn make_payload(size: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let mut state = if seed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        seed
    };
    for byte in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = b'A' + ((state & 0x0F) as u8);
    }
    out
}
