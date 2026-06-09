use std::fs;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::SecretKey;
use mars_xlog_core::decoder::{decode_xlog, DecodeOptions};
use rand_core::OsRng;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "mars-xlog",
    version,
    about = "Decode Tencent Mars xlog files from the command line."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Input xlog file. Shorthand for `decode --input`.
    #[arg(long, global = true)]
    input: Option<PathBuf>,

    /// Output decoded log path. Shorthand for `decode --output`.
    #[arg(long, global = true)]
    output: Option<PathBuf>,

    /// 64-hex-character private key for encrypted xlog blocks.
    #[arg(long, alias = "private-key", global = true)]
    key: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Decode one xlog file.
    Decode(DecodeArgs),
    /// Generate a secp256k1 key pair compatible with Mars xlog encryption.
    GenKey,
}

#[derive(Debug, Args)]
struct DecodeArgs {
    /// Input xlog file.
    #[arg(short = 'i', long)]
    input: Option<PathBuf>,
    /// Output decoded log path.
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,
    /// 64-hex-character private key for encrypted xlog blocks.
    #[arg(short = 'p', long, alias = "private-key")]
    key: Option<String>,
}

#[derive(Debug, Error)]
enum CliError {
    #[error("--input is required")]
    MissingInput,
    #[error("--output is required")]
    MissingOutput,
    #[error("read input failed {path}: {source}")]
    ReadInput {
        path: String,
        source: std::io::Error,
    },
    #[error("write output failed {path}: {source}")]
    WriteOutput {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Decode(#[from] mars_xlog_core::decoder::DecodeError),
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Decode(args)) => decode_command(
            args.input.or(cli.input),
            args.output.or(cli.output),
            args.key.or(cli.key),
        ),
        Some(Command::GenKey) => {
            print_key_pair();
            Ok(())
        }
        None => decode_command(cli.input, cli.output, cli.key),
    }
}

fn decode_command(
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    key: Option<String>,
) -> Result<(), CliError> {
    let input = input.ok_or(CliError::MissingInput)?;
    let output = output.ok_or(CliError::MissingOutput)?;
    let bytes = fs::read(&input).map_err(|source| CliError::ReadInput {
        path: input.display().to_string(),
        source,
    })?;

    let mut opts = DecodeOptions::new();
    if let Some(key) = key.as_deref() {
        opts = opts.private_key_hex(key)?;
    }

    let mut decoded = Vec::with_capacity(bytes.len());
    let summary = decode_xlog(&bytes, &mut decoded, &opts)?;
    fs::write(&output, &decoded).map_err(|source| CliError::WriteOutput {
        path: output.display().to_string(),
        source,
    })?;

    eprintln!(
        "decoded {} block(s), wrote {} byte(s) to {}",
        summary.blocks,
        summary.output_bytes,
        output.display()
    );
    Ok(())
}

fn print_key_pair() {
    let secret = SecretKey::random(&mut OsRng);
    let private_key = secret.to_bytes();
    let point = secret.public_key().to_encoded_point(false);
    let bytes = point.as_bytes();

    println!("private_key: {}", hex::encode(private_key));
    println!("public_key: {}", hex::encode(&bytes[1..]));
}
