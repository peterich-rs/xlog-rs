use std::env;
use std::fs;
use std::path::PathBuf;

use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};

const USAGE: &str = "\
Generate deterministic xlog fixture data.

Usage:
  cargo run -p mars-xlog --example gen_fixture -- [options]

Options:
  --out-dir <path>        Output directory for xlog files (required)
  --prefix <name>         Xlog name prefix / instance name (required)
  --mode <async|sync>     Appender mode (default: async)
  --compress <zlib|zstd>  Compression mode (default: zlib)
  --count <n>             Number of records to emit (default: 16)
  --pub-key <hex>         Optional 128-hex public key for crypt mode
";

#[derive(Debug)]
struct Options {
    out_dir: PathBuf,
    prefix: String,
    mode: AppenderMode,
    compress: CompressMode,
    count: usize,
    pub_key: Option<String>,
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
    let mut prefix: Option<String> = None;
    let mut mode = AppenderMode::Async;
    let mut compress = CompressMode::Zlib;
    let mut count: usize = 16;
    let mut pub_key: Option<String> = None;

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
                let v = iter
                    .next()
                    .ok_or_else(|| "--prefix requires a value".to_string())?;
                prefix = Some(v);
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
            "--count" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--count requires a value".to_string())?;
                count = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --count value {v}: {e}"))?;
            }
            "--pub-key" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--pub-key requires a value".to_string())?;
                pub_key = Some(v);
            }
            unknown => return Err(format!("unknown argument: {unknown}\n\n{USAGE}")),
        }
    }

    let out_dir = out_dir.ok_or_else(|| format!("--out-dir is required\n\n{USAGE}"))?;
    let prefix = prefix.ok_or_else(|| format!("--prefix is required\n\n{USAGE}"))?;

    Ok(Options {
        out_dir,
        prefix,
        mode,
        compress,
        count,
        pub_key,
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

    let mut config = XlogConfig::new(
        opts.out_dir.to_string_lossy().to_string(),
        opts.prefix.clone(),
    )
    .mode(opts.mode)
    .compress_mode(opts.compress);

    if let Some(key) = opts.pub_key.clone() {
        config = config.pub_key(key);
    }

    let logger = Xlog::init(config, LogLevel::Verbose).map_err(|e| format!("xlog init: {e}"))?;

    for idx in 0..opts.count {
        let msg = format!("FIXTURE|{}|{:04}", opts.prefix, idx);
        logger.write_with_meta(
            LogLevel::Info,
            Some("fixture"),
            "fixture_writer.rs",
            "emit_fixture",
            1000 + idx as u32,
            &msg,
        );
    }

    logger.flush(true);
    drop(logger);

    let mut files: Vec<PathBuf> = fs::read_dir(&opts.out_dir)
        .map_err(|e| format!("read out dir failed {}: {e}", opts.out_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("xlog"))
        .collect();
    files.sort();

    if files.is_empty() {
        return Err("no .xlog files were generated".to_string());
    }

    for file in files {
        println!("{}", file.display());
    }

    Ok(())
}
