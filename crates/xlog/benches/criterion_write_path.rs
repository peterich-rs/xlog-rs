use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mars_xlog::{AppenderMode, CompressMode, LogLevel, Xlog, XlogConfig};
use tempfile::TempDir;

static NEXT_BENCH_ID: AtomicUsize = AtomicUsize::new(1);

struct BenchLogger {
    _dir: TempDir,
    logger: Xlog,
    message: String,
}

impl BenchLogger {
    fn new(label: &str, mode: AppenderMode, payload_size: usize) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let prefix = format!(
            "criterion-{}-{}",
            label,
            NEXT_BENCH_ID.fetch_add(1, Ordering::Relaxed)
        );
        let cfg = XlogConfig::new(dir.path().display().to_string(), prefix)
            .mode(mode)
            .compress_mode(CompressMode::Zlib)
            .compress_level(6);
        let logger = Xlog::init(cfg, LogLevel::Info).expect("init xlog");
        logger.set_max_file_size(0);
        Self {
            _dir: dir,
            logger,
            message: make_message(payload_size),
        }
    }
}

fn make_message(payload_size: usize) -> String {
    let mut message = String::with_capacity(payload_size + 48);
    message.push_str("BENCH|");
    while message.len() < payload_size {
        message.push_str("rust-xlog-benchmark-line|");
    }
    message.truncate(payload_size);
    message
}

fn bench_sync_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("public_write_flush_per_msg");
    for size in [96usize, 512] {
        let sync_ctx = BenchLogger::new("sync-flush", AppenderMode::Sync, size);
        let async_ctx = BenchLogger::new("async-flush", AppenderMode::Async, size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("sync_zlib", size), &size, |b, _| {
            b.iter(|| {
                sync_ctx.logger.write_with_meta(
                    LogLevel::Info,
                    Some("bench"),
                    "criterion_write_path.rs",
                    "bench_sync_write",
                    1,
                    black_box(sync_ctx.message.as_str()),
                );
                sync_ctx.logger.flush(true);
            });
        });

        group.bench_with_input(BenchmarkId::new("async_zlib", size), &size, |b, _| {
            b.iter(|| {
                async_ctx.logger.write_with_meta(
                    LogLevel::Info,
                    Some("bench"),
                    "criterion_write_path.rs",
                    "bench_sync_write",
                    1,
                    black_box(async_ctx.message.as_str()),
                );
                async_ctx.logger.flush(true);
            });
        });

        sync_ctx.logger.flush(true);
        async_ctx.logger.flush(true);
    }
    group.finish();
}

fn bench_async_batch_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("public_write_batch256_flush");
    const BATCH_WRITES: usize = 256;
    for size in [96usize, 512] {
        let sync_ctx = BenchLogger::new("sync-batch", AppenderMode::Sync, size);
        let async_ctx = BenchLogger::new("async-batch", AppenderMode::Async, size);
        group.throughput(Throughput::Bytes((size * BATCH_WRITES) as u64));
        group.bench_with_input(BenchmarkId::new("sync_zlib", size), &size, |b, _| {
            b.iter(|| {
                for _ in 0..BATCH_WRITES {
                    sync_ctx.logger.write_with_meta(
                        LogLevel::Info,
                        Some("bench"),
                        "criterion_write_path.rs",
                        "bench_async_batch_flush",
                        1,
                        black_box(sync_ctx.message.as_str()),
                    );
                }
                sync_ctx.logger.flush(true);
            });
        });

        group.bench_with_input(BenchmarkId::new("async_zlib", size), &size, |b, _| {
            b.iter(|| {
                for _ in 0..BATCH_WRITES {
                    async_ctx.logger.write_with_meta(
                        LogLevel::Info,
                        Some("bench"),
                        "criterion_write_path.rs",
                        "bench_async_batch_flush",
                        1,
                        black_box(async_ctx.message.as_str()),
                    );
                }
                async_ctx.logger.flush(true);
            });
        });

        sync_ctx.logger.flush(true);
        async_ctx.logger.flush(true);
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20);
    targets = bench_sync_write, bench_async_batch_flush
);
criterion_main!(benches);
