# xlog-rs

This workspace provides a Rust-native implementation of Tencent Mars `xlog`. The public Rust release surface is `mars-xlog`; legacy C/C++ support stays repository-local.

## Release surface

- `mars-xlog` is the release-facing Rust crate and the intended Cargo entry point for Rust users.
- `mars-xlog-core` is the implementation-layer crate used by `mars-xlog`.
- `mars-xlog-cli` provides the `mars-xlog` command-line decoder for xlog files.
- `mars-xlog-sys` and the platform binding crates remain repository-local support crates and are not part of the `mars-xlog` release surface.

## Migration status
- Default runtime path is Rust (`mars-xlog-core` + `mars-xlog`); C++ build is no longer part of default workspace build.
- UniFFI/JNI/Harmony wrappers are wired to the Rust backend by default.
- `mars-xlog-sys` remains as a legacy crate for compatibility verification and reference, but it is not wired into `mars-xlog`.

## Workspace crates
- `mars-xlog-core`: Rust runtime core (protocol/compress/crypto/mmap/appender).
- `mars-xlog`: safe Rust wrapper API and the default Rust integration surface.
- `mars-xlog-cli`: command-line decoder for Mars-compatible `.xlog` files.
- `mars-xlog-uniffi`: minimal UniFFI surface (Kotlin/Swift friendly).
- `mars-xlog-android-jni`: JNI bridge used by the Android example app.
- `oh-xlog`: Harmony/ohos N-API bindings.
- `mars-xlog-sys`: legacy raw FFI + native build (C/C++/ObjC++) crate.

## Flutter package
- `packages/xlog`: Flutter-native Dart FFI package using native assets and a Rust `cdylib`.
- The package includes an example diagnostics app for real logging cases, stress tests, log file inspection, and Prometheus metrics snapshots.
- The Flutter side is pinned through `fvm` in [`packages/xlog/.fvmrc`](./packages/xlog/.fvmrc).

## Build notes
- Published Rust crates currently target Rust 1.85 or newer.
- Default workspace build (`cargo build`) uses the Rust backend and does not require C++14/Boost toolchains.
- `mars-xlog-sys` is excluded from workspace `default-members`; build it explicitly when needed:
  - `cargo build -p mars-xlog-sys`
- Building `mars-xlog-sys` uses source path `./third_party/mars/mars` by default.
- Override Mars source with `MARS_SRC_DIR=/path/to/mars` (the `mars` directory inside the Mars repo).

## Mars submodule
This repository uses Tencent Mars as a git submodule at `third_party/mars` for compatibility tests, decoder scripts, and legacy FFI builds.
The legacy `mars-xlog-sys` build uses `third_party/mars/mars` (the Mars repo's `mars/` directory).

Initialize the submodule (first time):
```bash
git submodule update --init --recursive
```

Update the submodule to a newer commit:
```bash
git -C third_party/mars fetch
git -C third_party/mars checkout <tag-or-commit>
git add third_party/mars
```

## Example (Rust)
```rust
use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};

fn main() -> anyhow::Result<()> {
    let cfg = XlogConfig::new("/tmp/xlog", "demo")
        .mode(AppenderMode::Async)
        .compress_mode(CompressMode::Zlib)
        .compress_level(6);

    let logger = Xlog::init(cfg, LogLevel::Debug)?;
    logger.log(LogLevel::Info, None, "hello from rust");
    logger.flush(true);
    Ok(())
}
```

## CLI decoder

Decode a generated `.xlog` file:

```bash
mars-xlog decode --input app.xlog --output app.log --key <64-hex-private-key>
```

`--key` is only required for encrypted async blocks. Plaintext logs can be decoded with:

```bash
mars-xlog --input app.xlog --output app.log
```

Install options:

```bash
# Homebrew tap/cask install from this repository or a copied tap.
brew install --cask ./Casks/mars-xlog.rb

# npm installs a small Node wrapper and downloads the release binary.
npm install -g mars-xlog-cli
```

The Homebrew cask and npm package both consume GitHub Release archives named
`mars-xlog-v<version>-<target>.tar.gz`.

## Example (tracing + tracing-subscriber)
Enable feature `tracing` and build an `XlogLayer`:
```rust
use mars_xlog::{LogLevel, Xlog, XlogConfig, XlogLayer, XlogLayerConfig};
use tracing_subscriber::prelude::*;

fn init_tracing() -> anyhow::Result<mars_xlog::XlogLayerHandle> {
    let cfg = XlogConfig::new("/tmp/xlog", "demo");
    let logger = Xlog::init(cfg, LogLevel::Info)?;

    let (layer, handle) = XlogLayer::with_config(
        logger,
        XlogLayerConfig::new(LogLevel::Info).enabled(true),
    );

    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(handle)
}
```

You can toggle the layer dynamically (mobile-friendly):
```rust
handle.set_enabled(false);
handle.set_level(LogLevel::Warn);
```

## Example (Android JNI)
An Android app example that calls the `mars-xlog` crate via JNI lives at:
`examples/android-jni`. See its README for build steps.

## Notes
- `Xlog::log`/`Xlog::write` capture caller file/line but not function name. Use the `xlog!` macros (feature `macros`) or `write_with_meta` for full metadata.
- Log files are single-writer only: the Rust backend is not multi-process safe for a shared `(name_prefix, log_dir/cache_dir)` namespace and enforces this with lock files in each storage directory.
- iOS/macOS console behavior keeps a native shim to preserve `printf`/`NSLog`/`OSLog` semantics.
- Low-level/global appender APIs are available directly in `mars-xlog` (`appender_open`/`appender_close`/`flush_all`/`appender_write_with_meta_raw`).

## License
MIT. See `LICENSE` and `NOTICE`.
