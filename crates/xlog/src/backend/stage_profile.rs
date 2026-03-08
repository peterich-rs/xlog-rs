#[cfg(feature = "bench-internals")]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "bench-internals")]
use std::sync::OnceLock;

#[cfg(feature = "bench-internals")]
use crate::bench::{
    AsyncPendingBlockStats, RustAsyncStageStats, RustSyncStageStats, StageLatencyStats,
    ValueDistributionStats,
};

#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
#[derive(Copy, Clone)]
pub(super) struct SyncStageSample {
    pub(super) total_ns: u64,
    pub(super) format_ns: u64,
    pub(super) block_ns: u64,
    pub(super) engine_write_ns: u64,
}

#[derive(Default)]
pub(super) struct SyncBuildStage {
    pub(super) format_ns: u64,
    pub(super) block_ns: u64,
}

#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
#[derive(Copy, Clone)]
pub(super) struct AsyncStageSample {
    pub(super) total_ns: u64,
    pub(super) format_ns: u64,
    pub(super) checkout_ns: u64,
    pub(super) checkout_lock_ns: u64,
    pub(super) checkout_wait_ns: u64,
    pub(super) begin_pending_ns: u64,
    pub(super) append_ns: u64,
    pub(super) force_flush_ns: u64,
}

#[derive(Default)]
pub(super) struct AsyncBuildStage {
    pub(super) format_ns: u64,
    pub(super) checkout_ns: u64,
    pub(super) checkout_lock_ns: u64,
    pub(super) checkout_wait_ns: u64,
    pub(super) begin_pending_ns: u64,
    pub(super) append_ns: u64,
    pub(super) force_flush_ns: u64,
}

pub(super) struct AsyncWriteFrontProfile {
    pub(super) format_ns: u64,
    pub(super) enqueue_ns: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum AsyncPendingFinalizeReason {
    Unknown,
    Threshold,
    ExplicitFlush,
    FlushEvery,
    Timeout,
    Stop,
}

#[cfg(feature = "bench-internals")]
const HISTOGRAM_SUB_BUCKETS: usize = 16;
#[cfg(feature = "bench-internals")]
const HISTOGRAM_BUCKETS: usize = 1 + (u64::BITS as usize * HISTOGRAM_SUB_BUCKETS);

#[cfg(feature = "bench-internals")]
struct StageHistogram {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
}

#[cfg(feature = "bench-internals")]
impl StageHistogram {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.sum_ns.store(0, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
    }

    fn record(&self, value: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(value, Ordering::Relaxed);

        let mut current_max = self.max_ns.load(Ordering::Relaxed);
        while value > current_max {
            match self.max_ns.compare_exchange_weak(
                current_max,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current_max = observed,
            }
        }

        self.buckets[histogram_bucket_index(value)].fetch_add(1, Ordering::Relaxed);
    }

    fn take_snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; HISTOGRAM_BUCKETS];
        for (dst, src) in buckets.iter_mut().zip(&self.buckets) {
            *dst = src.swap(0, Ordering::Relaxed);
        }
        HistogramSnapshot {
            count: self.count.swap(0, Ordering::Relaxed),
            sum_ns: self.sum_ns.swap(0, Ordering::Relaxed),
            max_ns: self.max_ns.swap(0, Ordering::Relaxed),
            buckets,
        }
    }
}

#[cfg(feature = "bench-internals")]
struct HistogramSnapshot {
    count: u64,
    sum_ns: u64,
    max_ns: u64,
    buckets: [u64; HISTOGRAM_BUCKETS],
}

#[cfg(feature = "bench-internals")]
struct SyncStageProfiler {
    enabled: AtomicBool,
    total: StageHistogram,
    format: StageHistogram,
    block: StageHistogram,
    engine_write: StageHistogram,
}

#[cfg(feature = "bench-internals")]
impl SyncStageProfiler {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            total: StageHistogram::new(),
            format: StageHistogram::new(),
            block: StageHistogram::new(),
            engine_write: StageHistogram::new(),
        }
    }

    fn reset(&self) {
        self.total.reset();
        self.format.reset();
        self.block.reset();
        self.engine_write.reset();
    }
}

#[cfg(feature = "bench-internals")]
struct AsyncStageProfiler {
    enabled: AtomicBool,
    total: StageHistogram,
    format: StageHistogram,
    checkout: StageHistogram,
    checkout_lock: StageHistogram,
    checkout_wait: StageHistogram,
    begin_pending: StageHistogram,
    append: StageHistogram,
    force_flush: StageHistogram,
    queue_full_count: AtomicU64,
    block_send_count: AtomicU64,
    block_send_ns: AtomicU64,
    queue_depth_current: AtomicUsize,
    queue_depth_high_watermark: AtomicUsize,
    pending_block_lines: StageHistogram,
    pending_block_raw_input_bytes: StageHistogram,
    pending_block_payload_bytes: StageHistogram,
    pending_block_reason_threshold: AtomicU64,
    pending_block_reason_explicit_flush: AtomicU64,
    pending_block_reason_flush_every: AtomicU64,
    pending_block_reason_timeout: AtomicU64,
    pending_block_reason_stop: AtomicU64,
    pending_block_reason_unknown: AtomicU64,
}

#[cfg(feature = "bench-internals")]
impl AsyncStageProfiler {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            total: StageHistogram::new(),
            format: StageHistogram::new(),
            checkout: StageHistogram::new(),
            checkout_lock: StageHistogram::new(),
            checkout_wait: StageHistogram::new(),
            begin_pending: StageHistogram::new(),
            append: StageHistogram::new(),
            force_flush: StageHistogram::new(),
            queue_full_count: AtomicU64::new(0),
            block_send_count: AtomicU64::new(0),
            block_send_ns: AtomicU64::new(0),
            queue_depth_current: AtomicUsize::new(0),
            queue_depth_high_watermark: AtomicUsize::new(0),
            pending_block_lines: StageHistogram::new(),
            pending_block_raw_input_bytes: StageHistogram::new(),
            pending_block_payload_bytes: StageHistogram::new(),
            pending_block_reason_threshold: AtomicU64::new(0),
            pending_block_reason_explicit_flush: AtomicU64::new(0),
            pending_block_reason_flush_every: AtomicU64::new(0),
            pending_block_reason_timeout: AtomicU64::new(0),
            pending_block_reason_stop: AtomicU64::new(0),
            pending_block_reason_unknown: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.total.reset();
        self.format.reset();
        self.checkout.reset();
        self.checkout_lock.reset();
        self.checkout_wait.reset();
        self.begin_pending.reset();
        self.append.reset();
        self.force_flush.reset();
        self.queue_full_count.store(0, Ordering::Relaxed);
        self.block_send_count.store(0, Ordering::Relaxed);
        self.block_send_ns.store(0, Ordering::Relaxed);
        self.queue_depth_current.store(0, Ordering::Relaxed);
        self.queue_depth_high_watermark.store(0, Ordering::Relaxed);
        self.pending_block_lines.reset();
        self.pending_block_raw_input_bytes.reset();
        self.pending_block_payload_bytes.reset();
        self.pending_block_reason_threshold
            .store(0, Ordering::Relaxed);
        self.pending_block_reason_explicit_flush
            .store(0, Ordering::Relaxed);
        self.pending_block_reason_flush_every
            .store(0, Ordering::Relaxed);
        self.pending_block_reason_timeout
            .store(0, Ordering::Relaxed);
        self.pending_block_reason_stop.store(0, Ordering::Relaxed);
        self.pending_block_reason_unknown
            .store(0, Ordering::Relaxed);
    }
}

#[cfg(feature = "bench-internals")]
static SYNC_STAGE_PROFILER: OnceLock<SyncStageProfiler> = OnceLock::new();
#[cfg(feature = "bench-internals")]
static ASYNC_STAGE_PROFILER: OnceLock<AsyncStageProfiler> = OnceLock::new();

#[cfg(feature = "bench-internals")]
fn sync_stage_profiler() -> &'static SyncStageProfiler {
    SYNC_STAGE_PROFILER.get_or_init(SyncStageProfiler::new)
}

#[cfg(feature = "bench-internals")]
fn async_stage_profiler() -> &'static AsyncStageProfiler {
    ASYNC_STAGE_PROFILER.get_or_init(AsyncStageProfiler::new)
}

#[cfg(feature = "bench-internals")]
pub(super) fn sync_stage_profile_enabled() -> bool {
    sync_stage_profiler().enabled.load(Ordering::Relaxed)
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn sync_stage_profile_enabled() -> bool {
    false
}

#[cfg(feature = "bench-internals")]
pub(super) fn async_stage_profile_enabled() -> bool {
    async_stage_profiler().enabled.load(Ordering::Relaxed)
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn async_stage_profile_enabled() -> bool {
    false
}

#[cfg(feature = "bench-internals")]
pub(super) fn record_sync_stage_sample(sample: SyncStageSample) {
    let profiler = sync_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }
    profiler.total.record(sample.total_ns);
    profiler.format.record(sample.format_ns);
    profiler.block.record(sample.block_ns);
    profiler.engine_write.record(sample.engine_write_ns);
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_sync_stage_sample(_sample: SyncStageSample) {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_stage_sample(sample: AsyncStageSample) {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }
    profiler.total.record(sample.total_ns);
    profiler.format.record(sample.format_ns);
    profiler.checkout.record(sample.checkout_ns);
    profiler.checkout_lock.record(sample.checkout_lock_ns);
    profiler.checkout_wait.record(sample.checkout_wait_ns);
    profiler.begin_pending.record(sample.begin_pending_ns);
    profiler.append.record(sample.append_ns);
    profiler.force_flush.record(sample.force_flush_ns);
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_stage_sample(_sample: AsyncStageSample) {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_queue_full() {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }
    profiler.queue_full_count.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_queue_full() {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_block_send(block_ns: u64) {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }
    profiler.block_send_count.fetch_add(1, Ordering::Relaxed);
    profiler
        .block_send_ns
        .fetch_add(block_ns, Ordering::Relaxed);
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_block_send(_block_ns: u64) {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_enqueued(queue_capacity: usize) {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }

    let depth = profiler
        .queue_depth_current
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    let depth_for_high = depth.min(queue_capacity);
    let mut current_max = profiler.queue_depth_high_watermark.load(Ordering::Acquire);
    while depth_for_high > current_max {
        match profiler.queue_depth_high_watermark.compare_exchange_weak(
            current_max,
            depth_for_high,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(v) => current_max = v,
        }
    }
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_enqueued(_queue_capacity: usize) {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_dequeued() {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }
    let mut current = profiler.queue_depth_current.load(Ordering::Acquire);
    while current > 0 {
        match profiler.queue_depth_current.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(v) => current = v,
        }
    }
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_dequeued() {}

#[cfg(feature = "bench-internals")]
pub(super) fn record_async_pending_block(
    lines: u64,
    raw_input_bytes: u64,
    payload_bytes: u64,
    reason: AsyncPendingFinalizeReason,
) {
    let profiler = async_stage_profiler();
    if !profiler.enabled.load(Ordering::Relaxed) {
        return;
    }

    profiler.pending_block_lines.record(lines);
    profiler
        .pending_block_raw_input_bytes
        .record(raw_input_bytes);
    profiler.pending_block_payload_bytes.record(payload_bytes);

    match reason {
        AsyncPendingFinalizeReason::Threshold => profiler
            .pending_block_reason_threshold
            .fetch_add(1, Ordering::Relaxed),
        AsyncPendingFinalizeReason::ExplicitFlush => profiler
            .pending_block_reason_explicit_flush
            .fetch_add(1, Ordering::Relaxed),
        AsyncPendingFinalizeReason::FlushEvery => profiler
            .pending_block_reason_flush_every
            .fetch_add(1, Ordering::Relaxed),
        AsyncPendingFinalizeReason::Timeout => profiler
            .pending_block_reason_timeout
            .fetch_add(1, Ordering::Relaxed),
        AsyncPendingFinalizeReason::Stop => profiler
            .pending_block_reason_stop
            .fetch_add(1, Ordering::Relaxed),
        AsyncPendingFinalizeReason::Unknown => profiler
            .pending_block_reason_unknown
            .fetch_add(1, Ordering::Relaxed),
    };
}

#[cfg(not(feature = "bench-internals"))]
pub(super) fn record_async_pending_block(
    _lines: u64,
    _raw_input_bytes: u64,
    _payload_bytes: u64,
    _reason: AsyncPendingFinalizeReason,
) {
}

#[cfg(feature = "bench-internals")]
fn histogram_bucket_index(value: u64) -> usize {
    if value == 0 {
        return 0;
    }

    let msb = (u64::BITS - 1 - value.leading_zeros()) as usize;
    let base = 1u128 << msb;
    let offset = (((value as u128 - base) * HISTOGRAM_SUB_BUCKETS as u128) / base)
        .min((HISTOGRAM_SUB_BUCKETS - 1) as u128) as usize;
    1 + (msb * HISTOGRAM_SUB_BUCKETS) + offset
}

#[cfg(feature = "bench-internals")]
fn histogram_bucket_upper_bound(index: usize) -> u64 {
    if index == 0 {
        return 0;
    }

    let bucket = index - 1;
    let msb = bucket / HISTOGRAM_SUB_BUCKETS;
    let sub_bucket = bucket % HISTOGRAM_SUB_BUCKETS;
    let base = 1u128 << msb;
    let upper = base + (((sub_bucket + 1) as u128 * base) / HISTOGRAM_SUB_BUCKETS as u128);
    upper.min(u64::MAX as u128) as u64
}

#[cfg(feature = "bench-internals")]
fn percentile_from_histogram(snapshot: &HistogramSnapshot, per_mille: u64) -> u64 {
    if snapshot.count == 0 {
        return 0;
    }

    let target =
        ((snapshot.count as u128 * per_mille as u128).saturating_add(999) / 1000).max(1) as u64;
    let mut seen = 0u64;
    for (index, count) in snapshot.buckets.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen >= target {
            return histogram_bucket_upper_bound(index);
        }
    }
    snapshot.max_ns
}

#[cfg(feature = "bench-internals")]
fn stage_stats(snapshot: &HistogramSnapshot) -> StageLatencyStats {
    if snapshot.count == 0 {
        return StageLatencyStats::default();
    }

    StageLatencyStats {
        avg_ns: snapshot.sum_ns as f64 / snapshot.count as f64,
        p50_ns: percentile_from_histogram(snapshot, 500),
        p95_ns: percentile_from_histogram(snapshot, 950),
        p99_ns: percentile_from_histogram(snapshot, 990),
        max_ns: snapshot.max_ns,
    }
}

#[cfg(feature = "bench-internals")]
fn distribution_stats(snapshot: &HistogramSnapshot) -> ValueDistributionStats {
    if snapshot.count == 0 {
        return ValueDistributionStats::default();
    }

    ValueDistributionStats {
        avg: snapshot.sum_ns as f64 / snapshot.count as f64,
        p50: percentile_from_histogram(snapshot, 500),
        p95: percentile_from_histogram(snapshot, 950),
        p99: percentile_from_histogram(snapshot, 990),
        max: snapshot.max_ns,
    }
}

#[cfg(feature = "bench-internals")]
pub(super) fn set_sync_stage_profile_enabled(enabled: bool) {
    let profiler = sync_stage_profiler();
    profiler.enabled.store(false, Ordering::Relaxed);
    profiler.reset();
    profiler.enabled.store(enabled, Ordering::Relaxed);
}

#[cfg(feature = "bench-internals")]
pub(super) fn take_sync_stage_stats() -> Option<RustSyncStageStats> {
    let profiler = sync_stage_profiler();
    let total = profiler.total.take_snapshot();
    if total.count == 0 {
        profiler.format.reset();
        profiler.block.reset();
        profiler.engine_write.reset();
        return None;
    }

    let format = profiler.format.take_snapshot();
    let block = profiler.block.take_snapshot();
    let engine_write = profiler.engine_write.take_snapshot();

    Some(RustSyncStageStats {
        samples: total.count as usize,
        total: stage_stats(&total),
        format: stage_stats(&format),
        block: stage_stats(&block),
        engine_write: stage_stats(&engine_write),
    })
}

#[cfg(feature = "bench-internals")]
pub(super) fn set_async_stage_profile_enabled(enabled: bool) {
    let profiler = async_stage_profiler();
    profiler.enabled.store(false, Ordering::Relaxed);
    profiler.reset();
    profiler.enabled.store(enabled, Ordering::Relaxed);
}

#[cfg(feature = "bench-internals")]
pub(super) fn take_async_stage_stats() -> Option<RustAsyncStageStats> {
    let profiler = async_stage_profiler();
    let total = profiler.total.take_snapshot();
    if total.count == 0 {
        profiler.format.reset();
        profiler.checkout.reset();
        profiler.checkout_lock.reset();
        profiler.checkout_wait.reset();
        profiler.begin_pending.reset();
        profiler.append.reset();
        profiler.force_flush.reset();
        profiler.queue_full_count.store(0, Ordering::Relaxed);
        profiler.block_send_count.store(0, Ordering::Relaxed);
        profiler.block_send_ns.store(0, Ordering::Relaxed);
        profiler.queue_depth_current.store(0, Ordering::Relaxed);
        profiler
            .queue_depth_high_watermark
            .store(0, Ordering::Relaxed);
        profiler.pending_block_lines.reset();
        profiler.pending_block_raw_input_bytes.reset();
        profiler.pending_block_payload_bytes.reset();
        profiler
            .pending_block_reason_threshold
            .store(0, Ordering::Relaxed);
        profiler
            .pending_block_reason_explicit_flush
            .store(0, Ordering::Relaxed);
        profiler
            .pending_block_reason_flush_every
            .store(0, Ordering::Relaxed);
        profiler
            .pending_block_reason_timeout
            .store(0, Ordering::Relaxed);
        profiler
            .pending_block_reason_stop
            .store(0, Ordering::Relaxed);
        profiler
            .pending_block_reason_unknown
            .store(0, Ordering::Relaxed);
        return None;
    }

    let format = profiler.format.take_snapshot();
    let checkout = profiler.checkout.take_snapshot();
    let checkout_lock = profiler.checkout_lock.take_snapshot();
    let checkout_wait = profiler.checkout_wait.take_snapshot();
    let begin_pending = profiler.begin_pending.take_snapshot();
    let append = profiler.append.take_snapshot();
    let force_flush = profiler.force_flush.take_snapshot();
    let pending_block_lines = profiler.pending_block_lines.take_snapshot();
    let pending_block_raw_input_bytes = profiler.pending_block_raw_input_bytes.take_snapshot();
    let pending_block_payload_bytes = profiler.pending_block_payload_bytes.take_snapshot();
    let block_send_count = profiler.block_send_count.swap(0, Ordering::Relaxed);
    let samples = total.count as usize;

    Some(RustAsyncStageStats {
        samples,
        total: stage_stats(&total),
        format: stage_stats(&format),
        checkout: stage_stats(&checkout),
        checkout_lock: stage_stats(&checkout_lock),
        checkout_wait: stage_stats(&checkout_wait),
        begin_pending: stage_stats(&begin_pending),
        append: stage_stats(&append),
        force_flush: stage_stats(&force_flush),
        queue_full_count: profiler.queue_full_count.swap(0, Ordering::Relaxed),
        block_send_count,
        block_send_ratio: if samples == 0 {
            0.0
        } else {
            block_send_count as f64 / samples as f64
        },
        block_send_ns: profiler.block_send_ns.swap(0, Ordering::Relaxed),
        queue_depth_high_watermark: profiler
            .queue_depth_high_watermark
            .swap(0, Ordering::Relaxed) as u64,
        flush_requeue_count: 0,
        pending_blocks: AsyncPendingBlockStats {
            finalized_blocks: pending_block_lines.count,
            total_lines: pending_block_lines.sum_ns,
            total_raw_input_bytes: pending_block_raw_input_bytes.sum_ns,
            total_payload_bytes: pending_block_payload_bytes.sum_ns,
            lines_per_block: distribution_stats(&pending_block_lines),
            raw_input_bytes_per_block: distribution_stats(&pending_block_raw_input_bytes),
            payload_bytes_per_block: distribution_stats(&pending_block_payload_bytes),
            finalized_by_threshold: profiler
                .pending_block_reason_threshold
                .swap(0, Ordering::Relaxed),
            finalized_by_explicit_flush: profiler
                .pending_block_reason_explicit_flush
                .swap(0, Ordering::Relaxed),
            finalized_by_flush_every: profiler
                .pending_block_reason_flush_every
                .swap(0, Ordering::Relaxed),
            finalized_by_timeout: profiler
                .pending_block_reason_timeout
                .swap(0, Ordering::Relaxed),
            finalized_by_stop: profiler
                .pending_block_reason_stop
                .swap(0, Ordering::Relaxed),
            finalized_by_unknown: profiler
                .pending_block_reason_unknown
                .swap(0, Ordering::Relaxed),
        },
    })
}

#[cfg(all(test, feature = "bench-internals"))]
mod tests {
    use super::*;

    #[test]
    fn stage_histogram_tracks_monotonic_percentiles() {
        let histogram = StageHistogram::new();
        for value in [120u64, 240, 480, 960, 1920, 3840] {
            histogram.record(value);
        }

        let snapshot = histogram.take_snapshot();
        let stats = stage_stats(&snapshot);
        assert_eq!(snapshot.count, 6);
        assert!(stats.avg_ns >= 120.0);
        assert!(stats.p50_ns <= stats.p95_ns);
        assert!(stats.p95_ns <= stats.p99_ns);
        assert!(stats.max_ns >= 3840);
    }

    #[test]
    fn distribution_histogram_tracks_monotonic_percentiles() {
        let histogram = StageHistogram::new();
        for value in [3u64, 7, 11, 19, 23, 29] {
            histogram.record(value);
        }

        let snapshot = histogram.take_snapshot();
        let stats = distribution_stats(&snapshot);
        assert_eq!(snapshot.count, 6);
        assert!(stats.avg >= 3.0);
        assert!(stats.p50 <= stats.p95);
        assert!(stats.p95 <= stats.p99);
        assert!(stats.max >= 29);
    }

    #[test]
    fn async_pending_block_stats_include_reason_counts() {
        set_async_stage_profile_enabled(true);
        record_async_stage_sample(AsyncStageSample {
            total_ns: 100,
            format_ns: 10,
            checkout_ns: 10,
            checkout_lock_ns: 0,
            checkout_wait_ns: 0,
            begin_pending_ns: 10,
            append_ns: 70,
            force_flush_ns: 0,
        });
        record_async_pending_block(4, 400, 120, AsyncPendingFinalizeReason::FlushEvery);
        record_async_pending_block(8, 800, 200, AsyncPendingFinalizeReason::Threshold);

        let stats = take_async_stage_stats().expect("async stats");
        assert_eq!(stats.pending_blocks.finalized_blocks, 2);
        assert_eq!(stats.pending_blocks.total_lines, 12);
        assert_eq!(stats.pending_blocks.total_raw_input_bytes, 1200);
        assert_eq!(stats.pending_blocks.total_payload_bytes, 320);
        assert_eq!(stats.pending_blocks.finalized_by_flush_every, 1);
        assert_eq!(stats.pending_blocks.finalized_by_threshold, 1);
    }
}
