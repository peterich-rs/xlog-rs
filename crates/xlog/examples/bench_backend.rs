use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Instant;

use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};

const USAGE: &str = "\
Benchmark xlog backend write throughput and latency.

Usage:
  cargo run -p mars-xlog --example bench_backend -- [options]

Options:
  --out-dir <path>         Output directory for logs (required)
  --prefix <name>          Log file prefix (default: bench)
  --messages <n>           Number of log records (default: 20000)
  --mode <async|sync>      Appender mode (default: async)
  --compress <zlib|zstd>   Compression mode (default: zlib)
  --msg-size <n>           Payload size per line (default: 96)
  --threads <n>            Worker threads (default: 1)
  --flush-every <n>        Call async flush every N writes per thread (default: 0 = disabled)
  --cache-dir <path>       Optional cache directory for mmap/log cache routing
  --cache-days <n>         Cache retention days when --cache-dir is set (default: 0)
  --max-file-size <n>      Max logfile size in bytes (default: 0 = disabled)
";

#[derive(Debug)]
struct Options {
    out_dir: PathBuf,
    prefix: String,
    messages: usize,
    mode: AppenderMode,
    compress: CompressMode,
    msg_size: usize,
    threads: usize,
    flush_every: usize,
    cache_dir: Option<PathBuf>,
    cache_days: i32,
    max_file_size: i64,
}

fn parse_mode(input: &str) -> Result<AppenderMode, String> {
    match input {
        "async" => Ok(AppenderMode::Async),
        "sync" => Ok(AppenderMode::Sync),
        _ => Err(format!("invalid --mode value: {input}")),
    }
}

fn parse_compress(input: &str) -> Result<CompressMode, String> {
    match input {
        "zlib" => Ok(CompressMode::Zlib),
        "zstd" => Ok(CompressMode::Zstd),
        _ => Err(format!("invalid --compress value: {input}")),
    }
}

fn parse_args() -> Result<Options, String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut prefix = "bench".to_string();
    let mut messages: usize = 20_000;
    let mut mode = AppenderMode::Async;
    let mut compress = CompressMode::Zlib;
    let mut msg_size = 96usize;
    let mut threads = 1usize;
    let mut flush_every = 0usize;
    let mut cache_dir: Option<PathBuf> = None;
    let mut cache_days = 0i32;
    let mut max_file_size = 0i64;

    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            "--out-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--out-dir requires a value".to_string())?;
                out_dir = Some(PathBuf::from(v));
            }
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| "--prefix requires a value".to_string())?;
            }
            "--messages" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--messages requires a value".to_string())?;
                messages = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --messages value {v}: {e}"))?;
            }
            "--mode" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--mode requires a value".to_string())?;
                mode = parse_mode(&v)?;
            }
            "--compress" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--compress requires a value".to_string())?;
                compress = parse_compress(&v)?;
            }
            "--msg-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--msg-size requires a value".to_string())?;
                msg_size = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --msg-size value {v}: {e}"))?;
            }
            "--threads" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--threads requires a value".to_string())?;
                threads = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --threads value {v}: {e}"))?;
            }
            "--flush-every" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--flush-every requires a value".to_string())?;
                flush_every = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --flush-every value {v}: {e}"))?;
            }
            "--cache-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--cache-dir requires a value".to_string())?;
                cache_dir = Some(PathBuf::from(v));
            }
            "--cache-days" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--cache-days requires a value".to_string())?;
                cache_days = v
                    .parse::<i32>()
                    .map_err(|e| format!("invalid --cache-days value {v}: {e}"))?;
            }
            "--max-file-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--max-file-size requires a value".to_string())?;
                max_file_size = v
                    .parse::<i64>()
                    .map_err(|e| format!("invalid --max-file-size value {v}: {e}"))?;
            }
            unknown => return Err(format!("unknown argument: {unknown}\n\n{USAGE}")),
        }
    }

    let out_dir = out_dir.ok_or_else(|| format!("--out-dir is required\n\n{USAGE}"))?;
    if messages == 0 {
        return Err("--messages must be > 0".to_string());
    }
    if msg_size == 0 {
        return Err("--msg-size must be > 0".to_string());
    }
    if threads == 0 {
        return Err("--threads must be > 0".to_string());
    }
    if cache_dir.is_none() && cache_days != 0 {
        return Err("--cache-days requires --cache-dir".to_string());
    }
    if max_file_size < 0 {
        return Err("--max-file-size must be >= 0".to_string());
    }

    Ok(Options {
        out_dir,
        prefix,
        messages,
        mode,
        compress,
        msg_size,
        threads,
        flush_every,
        cache_dir,
        cache_days,
        max_file_size,
    })
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("create out dir failed {}: {e}", opts.out_dir.display()))?;
    if let Some(cache_dir) = &opts.cache_dir {
        fs::create_dir_all(cache_dir)
            .map_err(|e| format!("create cache dir failed {}: {e}", cache_dir.display()))?;
    }

    let mut cfg = XlogConfig::new(opts.out_dir.to_string_lossy().to_string(), opts.prefix.clone())
        .mode(opts.mode)
        .compress_mode(opts.compress);
    if let Some(cache_dir) = &opts.cache_dir {
        cfg = cfg
            .cache_dir(cache_dir.to_string_lossy().to_string())
            .cache_days(opts.cache_days);
    }
    let logger = Xlog::init(cfg, LogLevel::Info).map_err(|e| format!("xlog init: {e}"))?;
    if opts.max_file_size > 0 {
        logger.set_max_file_size(opts.max_file_size);
    }

    let body = "X".repeat(opts.msg_size);
    let total_begin = Instant::now();
    let mut handles = Vec::with_capacity(opts.threads);
    let base = opts.messages / opts.threads;
    let extra = opts.messages % opts.threads;

    for thread_idx in 0..opts.threads {
        let logger = logger.clone();
        let body = body.clone();
        let thread_messages = base + usize::from(thread_idx < extra);
        let start_idx = thread_idx * base + thread_idx.min(extra);
        let flush_every = opts.flush_every;
        handles.push(thread::spawn(move || {
            let mut lat_ns = Vec::with_capacity(thread_messages);
            for local_idx in 0..thread_messages {
                let global_idx = start_idx + local_idx;
                let msg = format!("BENCH|T{thread_idx:02}|{:06}|{}", global_idx, body);
                let begin = Instant::now();
                logger.write_with_meta(
                    LogLevel::Info,
                    Some("bench"),
                    "bench_backend.rs",
                    "emit",
                    1,
                    &msg,
                );
                lat_ns.push(begin.elapsed().as_nanos() as u64);
                if flush_every > 0 && (local_idx + 1) % flush_every == 0 {
                    logger.flush(false);
                }
            }
            lat_ns
        }));
    }

    let mut lat_ns = Vec::with_capacity(opts.messages);
    for handle in handles {
        let mut chunk = handle
            .join()
            .map_err(|_| "bench worker thread panicked".to_string())?;
        lat_ns.append(&mut chunk);
    }

    logger.flush(true);
    drop(logger);
    let total_elapsed = total_begin.elapsed();

    lat_ns.sort_unstable();
    let p50 = percentile(&lat_ns, 50);
    let p95 = percentile(&lat_ns, 95);
    let p99 = percentile(&lat_ns, 99);
    let avg = lat_ns.iter().copied().sum::<u64>() as f64 / lat_ns.len() as f64;
    let throughput = opts.messages as f64 / total_elapsed.as_secs_f64();

    println!(
        "{{\"backend\":\"{}\",\"messages\":{},\"threads\":{},\"flush_every\":{},\"elapsed_ms\":{:.3},\"throughput_mps\":{:.3},\"lat_avg_ns\":{:.3},\"lat_p50_ns\":{},\"lat_p95_ns\":{},\"lat_p99_ns\":{}}}",
        backend_name(),
        opts.messages,
        opts.threads,
        opts.flush_every,
        total_elapsed.as_secs_f64() * 1000.0,
        throughput,
        avg,
        p50,
        p95,
        p99
    );

    Ok(())
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * p) / 100;
    sorted[idx]
}

fn backend_name() -> &'static str {
    #[cfg(feature = "rust-backend")]
    {
        "rust"
    }
    #[cfg(feature = "cpp-backend")]
    {
        "cpp"
    }
}
