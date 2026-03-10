use std::time::Duration;

#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};

#[cfg(feature = "metrics")]
pub(crate) fn record_engine_mode_switch(mode: &'static str) {
    counter!("xlog.core.engine.mode_switch_total", "mode" => mode).increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_engine_mode_switch(_mode: &'static str) {}

#[cfg(feature = "metrics")]
pub(crate) fn record_engine_write_block(
    mode: &'static str,
    bytes: usize,
    elapsed: Duration,
    force_flush: bool,
) {
    counter!("xlog.core.engine.write_block_total", "mode" => mode).increment(1);
    histogram!("xlog.core.engine.write_block_bytes", "mode" => mode).record(bytes as f64);
    histogram!("xlog.core.engine.write_block_ns", "mode" => mode).record(elapsed.as_nanos() as f64);
    if force_flush {
        counter!("xlog.core.engine.write_block_force_flush_total", "mode" => mode).increment(1);
    }
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_engine_write_block(
    _mode: &'static str,
    _bytes: usize,
    _elapsed: Duration,
    _force_flush: bool,
) {
}

#[cfg(feature = "metrics")]
pub(crate) fn record_engine_flush(
    mode: &'static str,
    reason: &'static str,
    elapsed: Duration,
    synced: bool,
) {
    counter!("xlog.core.engine.flush_total", "mode" => mode, "reason" => reason).increment(1);
    histogram!("xlog.core.engine.flush_ns", "mode" => mode, "reason" => reason)
        .record(elapsed.as_nanos() as f64);
    if synced {
        counter!("xlog.core.engine.flush_sync_total", "mode" => mode).increment(1);
    }
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_engine_flush(
    _mode: &'static str,
    _reason: &'static str,
    _elapsed: Duration,
    _synced: bool,
) {
}

#[cfg(feature = "metrics")]
pub(crate) fn record_engine_flush_requeue() {
    counter!("xlog.core.engine.flush_requeue_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_engine_flush_requeue() {}

#[cfg(feature = "metrics")]
pub(crate) fn record_engine_timeout_flush() {
    counter!("xlog.core.engine.timeout_flush_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_engine_timeout_flush() {}

#[cfg(feature = "metrics")]
pub(crate) fn record_async_buffer_len(used: usize, capacity: usize) {
    gauge!("xlog.core.async.buffer.used_bytes").set(used as f64);
    gauge!("xlog.core.async.buffer.capacity_bytes").set(capacity as f64);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_async_buffer_len(_used: usize, _capacity: usize) {}

#[cfg(feature = "metrics")]
pub(crate) fn record_async_buffer_append_failed() {
    counter!("xlog.core.async.buffer.append_failed_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_async_buffer_append_failed() {}

#[cfg(feature = "metrics")]
pub(crate) fn record_async_buffer_persisted(reason: &'static str) {
    counter!("xlog.core.async.buffer.persisted_total", "reason" => reason).increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_async_buffer_persisted(_reason: &'static str) {}

#[cfg(feature = "metrics")]
pub(crate) fn record_recovery_scan(valid_len: usize, recovered_pending: bool) {
    histogram!("xlog.core.async.recovery.valid_len_bytes").record(valid_len as f64);
    if recovered_pending {
        counter!("xlog.core.async.recovery.repaired_total").increment(1);
    }
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_recovery_scan(_valid_len: usize, _recovered_pending: bool) {}

#[cfg(feature = "metrics")]
pub(crate) fn record_file_append(bytes: usize, elapsed: Duration, keep_open: bool) {
    counter!("xlog.core.file.append_total").increment(1);
    histogram!("xlog.core.file.append_bytes").record(bytes as f64);
    histogram!("xlog.core.file.append_ns").record(elapsed.as_nanos() as f64);
    if keep_open {
        counter!("xlog.core.file.append_keep_open_total").increment(1);
    }
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_file_append(_bytes: usize, _elapsed: Duration, _keep_open: bool) {}

#[cfg(feature = "metrics")]
pub(crate) fn record_file_rotate() {
    counter!("xlog.core.file.rotate_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_file_rotate() {}

#[cfg(feature = "metrics")]
pub(crate) fn record_cache_move() {
    counter!("xlog.core.file.cache_move_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_cache_move() {}

#[cfg(feature = "metrics")]
pub(crate) fn record_expired_delete() {
    counter!("xlog.core.file.expired_delete_total").increment(1);
}

#[cfg(not(feature = "metrics"))]
pub(crate) fn record_expired_delete() {}
