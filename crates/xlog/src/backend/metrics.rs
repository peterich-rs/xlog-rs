pub(super) const METRICS_ENABLED: bool = cfg!(feature = "metrics");

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
    Timeout,
    Stop,
}

#[cfg(feature = "metrics")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};

#[cfg(feature = "metrics")]
static ASYNC_QUEUE_DEPTH_CURRENT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "metrics")]
static ASYNC_QUEUE_DEPTH_HIGH_WATERMARK: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "metrics")]
pub(super) fn record_sync_stage_sample(sample: SyncStageSample) {
    counter!("xlog.sync.stage.sample_total").increment(1);
    histogram!("xlog.sync.stage.total_ns").record(sample.total_ns as f64);
    histogram!("xlog.sync.stage.format_ns").record(sample.format_ns as f64);
    histogram!("xlog.sync.stage.block_ns").record(sample.block_ns as f64);
    histogram!("xlog.sync.stage.engine_write_ns").record(sample.engine_write_ns as f64);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_sync_stage_sample(_sample: SyncStageSample) {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_stage_sample(sample: AsyncStageSample) {
    counter!("xlog.async.stage.sample_total").increment(1);
    histogram!("xlog.async.stage.total_ns").record(sample.total_ns as f64);
    histogram!("xlog.async.stage.format_ns").record(sample.format_ns as f64);
    histogram!("xlog.async.stage.checkout_ns").record(sample.checkout_ns as f64);
    histogram!("xlog.async.stage.checkout_lock_ns").record(sample.checkout_lock_ns as f64);
    histogram!("xlog.async.stage.checkout_wait_ns").record(sample.checkout_wait_ns as f64);
    histogram!("xlog.async.stage.begin_pending_ns").record(sample.begin_pending_ns as f64);
    histogram!("xlog.async.stage.append_ns").record(sample.append_ns as f64);
    histogram!("xlog.async.stage.force_flush_ns").record(sample.force_flush_ns as f64);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_stage_sample(_sample: AsyncStageSample) {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_queue_full() {
    counter!("xlog.async.queue.full_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_queue_full() {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_block_send(block_ns: u64) {
    counter!("xlog.async.queue.block_send_total").increment(1);
    histogram!("xlog.async.queue.block_send_ns").record(block_ns as f64);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_block_send(_block_ns: u64) {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_enqueued(queue_capacity: usize) {
    let depth = ASYNC_QUEUE_DEPTH_CURRENT
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    let capped = depth.min(queue_capacity);
    let mut current_max = ASYNC_QUEUE_DEPTH_HIGH_WATERMARK.load(Ordering::Acquire);
    while capped > current_max {
        match ASYNC_QUEUE_DEPTH_HIGH_WATERMARK.compare_exchange_weak(
            current_max,
            capped,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(v) => current_max = v,
        }
    }

    gauge!("xlog.async.queue.depth").set(depth as f64);
    gauge!("xlog.async.queue.depth_high_watermark")
        .set(ASYNC_QUEUE_DEPTH_HIGH_WATERMARK.load(Ordering::Acquire) as f64);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_enqueued(_queue_capacity: usize) {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_dequeued() {
    let mut current = ASYNC_QUEUE_DEPTH_CURRENT.load(Ordering::Acquire);
    while current > 0 {
        match ASYNC_QUEUE_DEPTH_CURRENT.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                current -= 1;
                break;
            }
            Err(v) => current = v,
        }
    }
    gauge!("xlog.async.queue.depth").set(current as f64);
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_dequeued() {}

#[cfg(feature = "metrics")]
pub(super) fn record_async_pending_block(
    lines: u64,
    raw_input_bytes: u64,
    payload_bytes: u64,
    reason: AsyncPendingFinalizeReason,
) {
    counter!("xlog.async.pending.finalized_total").increment(1);
    histogram!("xlog.async.pending.lines_per_block").record(lines as f64);
    histogram!("xlog.async.pending.raw_input_bytes_per_block").record(raw_input_bytes as f64);
    histogram!("xlog.async.pending.payload_bytes_per_block").record(payload_bytes as f64);

    match reason {
        AsyncPendingFinalizeReason::Threshold => counter!(
            "xlog.async.pending.finalized_by_reason_total",
            "reason" => "threshold"
        )
        .increment(1),
        AsyncPendingFinalizeReason::ExplicitFlush => counter!(
            "xlog.async.pending.finalized_by_reason_total",
            "reason" => "explicit_flush"
        )
        .increment(1),
        AsyncPendingFinalizeReason::Timeout => counter!(
            "xlog.async.pending.finalized_by_reason_total",
            "reason" => "timeout"
        )
        .increment(1),
        AsyncPendingFinalizeReason::Stop => counter!(
            "xlog.async.pending.finalized_by_reason_total",
            "reason" => "stop"
        )
        .increment(1),
        AsyncPendingFinalizeReason::Unknown => counter!(
            "xlog.async.pending.finalized_by_reason_total",
            "reason" => "unknown"
        )
        .increment(1),
    };
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_pending_block(
    _lines: u64,
    _raw_input_bytes: u64,
    _payload_bytes: u64,
    _reason: AsyncPendingFinalizeReason,
) {
}

#[cfg(feature = "metrics")]
pub(super) fn record_async_flush_requeues(count: u64) {
    if count > 0 {
        counter!("xlog.async.flush.requeue_total").increment(count);
    }
}

#[cfg(not(feature = "metrics"))]
pub(super) fn record_async_flush_requeues(_count: u64) {}
