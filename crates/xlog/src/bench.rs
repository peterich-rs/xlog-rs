//! Benchmark-only Rust backend profiling helpers.
//!
//! This module is gated behind the `bench-internals` feature so that release
//! consumers do not see experimental profiling hooks in the default API
//! surface. The types here are intended for local performance analysis and may
//! change along with the benchmark harness.

/// Stage-level latency summary used by Rust backend sync/async profiling.
///
/// `avg_ns`, `max_ns`, and `samples` are exact aggregates over the profiling
/// window. `p50_ns`, `p95_ns`, and `p99_ns` are approximate percentile upper
/// bounds derived from the internal fixed-bucket histogram used by the
/// low-overhead profiler.
#[derive(Debug, Clone, Default)]
pub struct StageLatencyStats {
    pub avg_ns: f64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// Histogram-based value distribution used by benchmark-only diagnostics.
///
/// `avg`, `max`, and the total counts derived from these stats are exact.
/// `p50`, `p95`, and `p99` are approximate percentile upper bounds derived
/// from the same low-overhead fixed-bucket histogram used by stage profiling.
#[derive(Debug, Clone, Default)]
pub struct ValueDistributionStats {
    pub avg: f64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// Aggregated async pending-block stats for the Rust backend.
#[derive(Debug, Clone, Default)]
pub struct AsyncPendingBlockStats {
    pub finalized_blocks: u64,
    pub total_lines: u64,
    pub total_raw_input_bytes: u64,
    pub total_payload_bytes: u64,
    pub lines_per_block: ValueDistributionStats,
    pub raw_input_bytes_per_block: ValueDistributionStats,
    pub payload_bytes_per_block: ValueDistributionStats,
    pub finalized_by_threshold: u64,
    pub finalized_by_explicit_flush: u64,
    pub finalized_by_flush_every: u64,
    pub finalized_by_timeout: u64,
    pub finalized_by_stop: u64,
    pub finalized_by_unknown: u64,
}

/// Aggregated sync hot-path stage stats for the Rust backend.
#[derive(Debug, Clone, Default)]
pub struct RustSyncStageStats {
    pub samples: usize,
    pub total: StageLatencyStats,
    pub format: StageLatencyStats,
    pub block: StageLatencyStats,
    pub engine_write: StageLatencyStats,
}

/// Aggregated async hot-path stage stats for the Rust backend.
#[derive(Debug, Clone, Default)]
pub struct RustAsyncStageStats {
    pub samples: usize,
    pub total: StageLatencyStats,
    pub format: StageLatencyStats,
    pub checkout: StageLatencyStats,
    pub checkout_lock: StageLatencyStats,
    pub checkout_wait: StageLatencyStats,
    pub begin_pending: StageLatencyStats,
    pub append: StageLatencyStats,
    pub force_flush: StageLatencyStats,
    pub queue_full_count: u64,
    pub block_send_count: u64,
    pub block_send_ratio: f64,
    pub block_send_ns: u64,
    pub queue_depth_high_watermark: u64,
    pub flush_requeue_count: u64,
    pub pending_blocks: AsyncPendingBlockStats,
}

/// Enable or disable Rust backend sync stage profiling.
///
/// When enabled, each sync write records stage timing for:
/// `format -> block build -> engine write`.
pub fn set_rust_sync_stage_profile_enabled(enabled: bool) {
    crate::backend::set_rust_sync_stage_profile_enabled(enabled);
}

/// Enable or disable Rust backend async stage profiling.
///
/// When enabled, each async write records stage timing for:
/// `format -> checkout -> begin_pending -> append -> force_flush`.
pub fn set_rust_async_stage_profile_enabled(enabled: bool) {
    crate::backend::set_rust_async_stage_profile_enabled(enabled);
}

/// Mark the next async `flush(false)` issued on the current thread as a
/// benchmark `flush_every` control action.
///
/// This only affects benchmark-only profiling output and has no effect unless
/// `bench-internals` is enabled.
pub fn mark_rust_async_flush_hint_flush_every() {
    crate::backend::mark_rust_async_flush_hint_flush_every();
}

/// Consume Rust backend sync stage profiling stats.
///
/// Returns `None` if profiling is disabled or no samples were recorded.
pub fn take_rust_sync_stage_stats() -> Option<RustSyncStageStats> {
    crate::backend::take_rust_sync_stage_stats()
}

/// Consume Rust backend async stage profiling stats.
///
/// Returns `None` if profiling is disabled or no samples were recorded.
pub fn take_rust_async_stage_stats() -> Option<RustAsyncStageStats> {
    crate::backend::take_rust_async_stage_stats()
}
