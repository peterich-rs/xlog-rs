# mars-xlog-core

`mars-xlog-core` contains the low-level runtime building blocks used by
`mars-xlog`.

It includes:

- protocol encoding and decoding
- persistent mmap buffer management
- async/sync append engine primitives
- compression and encryption helpers
- file lifecycle and recovery utilities

This crate exists primarily as an implementation layer for the top-level Rust
API. Most external Rust users should start with `mars-xlog` instead.

## MSRV

`mars-xlog-core` currently targets Rust 1.85 or newer.

## Feature flags

- `metrics`: emits runtime metrics via the `metrics` crate (requires a recorder)

## Stability

`mars-xlog-core` is useful for internal composition and advanced integration,
but it is not the primary public surface of the project. Release notes and
examples should treat `mars-xlog` as the default entry point.

## License

MIT. See the repository root `LICENSE` and `NOTICE`.
