use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};

const USAGE: &str = "\
Benchmark xlog backend write throughput and latency.

Usage:
  cargo run -p mars-xlog --example bench_backend -- [options]

Options:
  --out-dir <path>        Output directory for logs (required)
  --prefix <name>         Log file prefix (default: bench)
  --messages <n>          Number of log records (default: 20000)
  --mode <async|sync>     Appender mode (default: async)
  --compress <zlib|zstd>  Compression mode (default: zlib)
  --msg-size <n>          Payload size per line (default: 96)
";

#[derive(Debug)]
struct Options {
    out_dir: PathBuf,
    prefix: String,
    messages: usize,
    mode: AppenderMode,
    compress: CompressMode,
    msg_size: usize,
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

    Ok(Options {
        out_dir,
        prefix,
        messages,
        mode,
        compress,
        msg_size,
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

    let cfg = XlogConfig::new(opts.out_dir.to_string_lossy().to_string(), opts.prefix)
        .mode(opts.mode)
        .compress_mode(opts.compress);
    let logger = Xlog::init(cfg, LogLevel::Info).map_err(|e| format!("xlog init: {e}"))?;

    let body = "X".repeat(opts.msg_size);
    let mut lat_ns = Vec::with_capacity(opts.messages);

    let total_begin = Instant::now();
    for idx in 0..opts.messages {
        let msg = format!("BENCH|{:06}|{}", idx, body);
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
        "{{\"backend\":\"{}\",\"messages\":{},\"elapsed_ms\":{:.3},\"throughput_mps\":{:.3},\"lat_avg_ns\":{:.3},\"lat_p50_ns\":{},\"lat_p95_ns\":{},\"lat_p99_ns\":{}}}",
        backend_name(),
        opts.messages,
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
    "rust"
}
