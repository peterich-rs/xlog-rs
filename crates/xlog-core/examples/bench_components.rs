use std::env;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

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
  --io-iterations <n> Iterations for file I/O path benchmarks (default: min(iterations, 5000))
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
    let io_iterations = io_iterations.unwrap_or(iterations.min(5_000)).max(1);

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

    let start = Instant::now();
    for _ in 0..opts.iterations {
        tea_encrypt_in_place(black_box(block.as_mut_slice()), black_box(&key));
    }
    let elapsed = start.elapsed();
    emit_result(
        "crypto",
        "tea_encrypt",
        opts.payload_size,
        opts.iterations,
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        input_bytes,
        1.0,
    );

    let start = Instant::now();
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
    let elapsed = start.elapsed();
    let input_bytes = 32usize.saturating_mul(opts.iterations);
    emit_result(
        "crypto",
        "ecdh_derive",
        32,
        opts.iterations,
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        input_bytes,
        1.0,
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
    let start = Instant::now();
    for _ in 0..opts.iterations {
        let line = format_record(black_box(&record), black_box(&payload));
        sink = sink.wrapping_add(line.len());
        black_box(&line);
    }
    black_box(sink);
    let elapsed = start.elapsed();
    let input_bytes = opts.payload_size.saturating_mul(opts.iterations);
    emit_result(
        "formatter",
        "format_record_alloc",
        opts.payload_size,
        opts.iterations,
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        sink,
        if input_bytes == 0 {
            0.0
        } else {
            sink as f64 / input_bytes as f64
        },
    );

    let mut out = String::with_capacity(16 * 1024);
    let mut sink_reuse = 0usize;
    let start = Instant::now();
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
    let elapsed = start.elapsed();
    emit_result(
        "formatter",
        "format_record_parts_into",
        opts.payload_size,
        opts.iterations,
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        sink_reuse,
        if input_bytes == 0 {
            0.0
        } else {
            sink_reuse as f64 / input_bytes as f64
        },
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
    bench_move_old_cache_files_variant(opts, payload.as_slice())?;
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
    let start = Instant::now();
    for _ in 0..opts.iterations {
        compressor
            .compress_chunk(black_box(payload), &mut compressed)
            .map_err(|e| format!("{variant} compress chunk failed: {e}"))?;
    }
    compressor
        .flush(&mut compressed)
        .map_err(|e| format!("{variant} flush failed: {e}"))?;
    let elapsed = start.elapsed();
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
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        output_bytes,
        ratio,
    );

    Ok(())
}

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

    let start = Instant::now();
    for _ in 0..opts.io_iterations {
        manager
            .append_log_bytes(
                black_box(payload),
                max_file_size,
                move_file,
                keep_open,
            )
            .map_err(|e| format!("{variant} append failed: {e}"))?;
    }
    let elapsed = start.elapsed();

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
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        output_bytes,
        ratio,
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

    let mut input_bytes = 0usize;
    let start = Instant::now();
    for idx in 0..opts.io_iterations {
        let path = cache_dir.join(format!("bench-move-{idx}.xlog"));
        fs::write(&path, payload).map_err(|e| format!("{variant} seed file write failed: {e}"))?;
        input_bytes = input_bytes.saturating_add(payload.len());
        manager
            .move_old_cache_files(0)
            .map_err(|e| format!("{variant} move failed: {e}"))?;
    }
    let elapsed = start.elapsed();

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
        opts.io_iterations,
        elapsed.as_secs_f64() * 1000.0,
        input_bytes,
        output_bytes,
        ratio,
    );
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

fn emit_result(
    component: &str,
    variant: &str,
    payload_size: usize,
    iterations: usize,
    elapsed_ms: f64,
    input_bytes: usize,
    output_bytes: usize,
    ratio: f64,
) {
    let elapsed_s = (elapsed_ms / 1000.0).max(1e-12);
    let ops_per_sec = iterations as f64 / elapsed_s;
    let bytes_per_sec = input_bytes as f64 / elapsed_s;
    println!(
        "{{\"component\":\"{}\",\"variant\":\"{}\",\"payload_size\":{},\"iterations\":{},\"elapsed_ms\":{:.3},\"ops_per_sec\":{:.3},\"bytes_per_sec\":{:.3},\"input_bytes\":{},\"output_bytes\":{},\"ratio\":{:.6}}}",
        component,
        variant,
        payload_size,
        iterations,
        elapsed_ms,
        ops_per_sec,
        bytes_per_sec,
        input_bytes,
        output_bytes,
        ratio
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
