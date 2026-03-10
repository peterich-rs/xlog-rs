//! Core building blocks for the Rust-native xlog engine.
//!
//! `mars-xlog-core` is the implementation layer behind the top-level
//! `mars-xlog` crate. Most external users should start with `mars-xlog` unless
//! they are intentionally composing lower-level buffer, protocol, or appender
//! primitives.

/// Append engine, flush control, and async pending-block primitives.
pub mod appender_engine;
/// Persistent mmap-backed buffer and recovery helpers.
pub mod buffer;
/// Compression helpers and streaming compressor implementations.
pub mod compress;
/// ECDH+TEA encryption helpers.
pub mod crypto;
/// Human-readable dump utilities for log buffers.
pub mod dump;
/// File lifecycle, cache movement, and active log append helpers.
pub mod file_manager;
/// Line formatter used by the Rust runtime path.
pub mod formatter;
mod metrics;
/// Thin mmap storage wrapper used by persistent buffers.
pub mod mmap_store;
/// One-shot flush path used to drain mmap/cache state into log files.
pub mod oneshot;
/// Platform console forwarding helpers.
pub mod platform_console;
/// Platform thread id helpers.
pub mod platform_tid;
/// Protocol constants and header helpers.
pub mod protocol;
/// Log record model used by formatter and runtime paths.
pub mod record;
/// Instance registry helpers.
pub mod registry;
