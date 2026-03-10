# mars-xlog

`mars-xlog` is the public Rust API for this repository's xlog implementation.

It provides:

- async and sync appenders
- zlib and zstd compression
- optional public-key encryption
- global appender helpers
- `tracing-subscriber` integration behind the `tracing` feature

This crate is the intended entry point for Rust users.

## MSRV

`mars-xlog` currently targets Rust 1.85 or newer.

## Quick start

```rust
use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};

fn main() -> anyhow::Result<()> {
    let cfg = XlogConfig::new("/tmp/xlog", "demo")
        .mode(AppenderMode::Async)
        .compress_mode(CompressMode::Zlib)
        .compress_level(6);

    let logger = Xlog::init(cfg, LogLevel::Info)?;
    logger.log(LogLevel::Info, Some("demo"), "hello from rust");
    logger.flush(true);
    Ok(())
}
```

## Feature flags

- `macros`: enables the `xlog!` family of call-site macros
- `tracing`: enables `XlogLayer` for `tracing-subscriber`
- `metrics`: emits runtime metrics via the `metrics` crate (requires a recorder)
- `metrics-prometheus`: enables the Prometheus recorder for bench/example usage

## Scope

This crate is the release-facing Rust surface.
Legacy C++ parity and benchmark support stay in the repository, but are not part
of the intended default Rust integration path.

## License

MIT. See the repository root `LICENSE` and `NOTICE`.
