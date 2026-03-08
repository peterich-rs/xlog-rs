use std::time::{Duration, SystemTime, UNIX_EPOCH};

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use mars_xlog_core::compress::{
    decompress_raw_zlib, decompress_zstd_frames, StreamCompressor, ZlibStreamCompressor,
    ZstdChunkCompressor, ZstdStreamCompressor,
};
use mars_xlog_core::crypto::{tea_encrypt_in_place, EcdhTeaCipher};
use mars_xlog_core::formatter::format_record_parts_into;
use mars_xlog_core::record::LogLevel;

const SAMPLE_PUBKEY: &str =
    "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8";

#[derive(Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let init = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: init }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

fn make_ascii_payload(len: usize, seed: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-/:,.";
    let mut rng = XorShift64::new(seed ^ len as u64);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        let idx = (rng.next_u64() as usize) % ALPHABET.len();
        out.push(ALPHABET[idx] as char);
    }
    out
}

fn make_binary_payload(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = XorShift64::new(seed ^ ((len as u64) << 16));
    let mut out = vec![0u8; len];
    for byte in &mut out {
        *byte = (rng.next_u64() & 0xFF) as u8;
    }
    out
}

fn fixed_ts() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn bench_formatter(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_formatter");
    for size in [96usize, 256, 1024] {
        let message = make_ascii_payload(size, 0xA5A5_0101);
        let mut out = String::with_capacity(size + 160);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("format_record_parts_into", size),
            &size,
            |b, _| {
                b.iter(|| {
                    out.clear();
                    format_record_parts_into(
                        &mut out,
                        LogLevel::Info,
                        black_box("bench"),
                        black_box("criterion_components.rs"),
                        black_box("formatter_bench"),
                        42,
                        fixed_ts(),
                        100,
                        200,
                        200,
                        black_box(message.as_str()),
                    );
                    black_box(out.len())
                });
            },
        );
    }
    group.finish();
}

fn bench_compression(c: &mut Criterion) {
    for size in [256usize, 1024, 4096] {
        let payload = make_binary_payload(size, 0xCAFE_BABE);
        let zlib_fixture = zlib_decode_fixture(size);
        let zstd_stream_fixture = zstd_stream_decode_fixture(size);
        let zstd_chunk_fixture = zstd_chunk_decode_fixture(size);

        assert_eq!(
            decompress_raw_zlib(zlib_fixture.bytes)
                .expect("zlib decode fixture validation")
                .len(),
            zlib_fixture.expected_len
        );
        assert_eq!(
            decompress_zstd_frames(zstd_stream_fixture.bytes)
                .expect("zstd stream decode fixture validation")
                .len(),
            zstd_stream_fixture.expected_len
        );
        assert_eq!(
            decompress_zstd_frames(zstd_chunk_fixture.bytes)
                .expect("zstd chunk decode fixture validation")
                .len(),
            zstd_chunk_fixture.expected_len
        );

        let mut encode_group = c.benchmark_group("core_compress_encode");
        encode_group.throughput(Throughput::Bytes(size as u64));

        encode_group.bench_with_input(BenchmarkId::new("zlib_stream_l6", size), &size, |b, _| {
            b.iter_batched(
                || {
                    (
                        ZlibStreamCompressor::new(6),
                        Vec::with_capacity(size),
                        payload.clone(),
                    )
                },
                |(mut compressor, mut out, input)| {
                    compressor
                        .compress_chunk(black_box(input.as_slice()), &mut out)
                        .expect("zlib compress");
                    compressor.flush(&mut out).expect("zlib flush");
                    black_box(out.len());
                },
                BatchSize::SmallInput,
            );
        });

        encode_group.bench_with_input(BenchmarkId::new("zstd_stream_l3", size), &size, |b, _| {
            b.iter_batched(
                || {
                    (
                        ZstdStreamCompressor::new(3).expect("zstd stream init"),
                        Vec::with_capacity(size),
                        payload.clone(),
                    )
                },
                |(mut compressor, mut out, input)| {
                    compressor
                        .compress_chunk(black_box(input.as_slice()), &mut out)
                        .expect("zstd stream compress");
                    compressor.flush(&mut out).expect("zstd stream flush");
                    black_box(out.len());
                },
                BatchSize::SmallInput,
            );
        });

        encode_group.bench_with_input(BenchmarkId::new("zstd_chunk_l3", size), &size, |b, _| {
            b.iter_batched(
                || {
                    (
                        ZstdChunkCompressor::new(3),
                        Vec::with_capacity(size),
                        payload.clone(),
                    )
                },
                |(mut compressor, mut out, input)| {
                    compressor
                        .compress_chunk(black_box(input.as_slice()), &mut out)
                        .expect("zstd chunk compress");
                    compressor.flush(&mut out).expect("zstd chunk flush");
                    black_box(out.len());
                },
                BatchSize::SmallInput,
            );
        });
        encode_group.finish();

        let mut decode_group = c.benchmark_group("core_compress_decode");
        decode_group.throughput(Throughput::Bytes(size as u64));

        decode_group.bench_with_input(BenchmarkId::new("zlib_stream_l6", size), &size, |b, _| {
            b.iter(|| {
                let decoded =
                    decompress_raw_zlib(black_box(zlib_fixture.bytes)).expect("zlib decompress");
                black_box(decoded.len());
            });
        });

        decode_group.bench_with_input(BenchmarkId::new("zstd_stream_l3", size), &size, |b, _| {
            b.iter(|| {
                let decoded = decompress_zstd_frames(black_box(zstd_stream_fixture.bytes))
                    .expect("zstd stream decompress");
                black_box(decoded.len());
            });
        });

        decode_group.bench_with_input(BenchmarkId::new("zstd_chunk_l3", size), &size, |b, _| {
            b.iter(|| {
                let decoded = decompress_zstd_frames(black_box(zstd_chunk_fixture.bytes))
                    .expect("zstd chunk decompress");
                black_box(decoded.len());
            });
        });
        decode_group.finish();
    }
}

struct DecodeFixture {
    bytes: &'static [u8],
    expected_len: usize,
}

fn zlib_decode_fixture(size: usize) -> DecodeFixture {
    match size {
        256 => DecodeFixture {
            bytes: include_bytes!("fixtures/zlib_stream_l6_256.bin"),
            expected_len: 256,
        },
        1024 => DecodeFixture {
            bytes: include_bytes!("fixtures/zlib_stream_l6_1024.bin"),
            expected_len: 1024,
        },
        4096 => DecodeFixture {
            bytes: include_bytes!("fixtures/zlib_stream_l6_4096.bin"),
            expected_len: 4096,
        },
        _ => panic!("unsupported zlib decode fixture size: {size}"),
    }
}

fn zstd_stream_decode_fixture(size: usize) -> DecodeFixture {
    match size {
        256 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_stream_l3_256.bin"),
            expected_len: 256,
        },
        1024 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_stream_l3_1024.bin"),
            expected_len: 1024,
        },
        4096 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_stream_l3_4096.bin"),
            expected_len: 4096,
        },
        _ => panic!("unsupported zstd stream decode fixture size: {size}"),
    }
}

fn zstd_chunk_decode_fixture(size: usize) -> DecodeFixture {
    match size {
        256 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_chunk_l3_256.bin"),
            expected_len: 256,
        },
        1024 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_chunk_l3_1024.bin"),
            expected_len: 1024,
        },
        4096 => DecodeFixture {
            bytes: include_bytes!("fixtures/zstd_chunk_l3_4096.bin"),
            expected_len: 4096,
        },
        _ => panic!("unsupported zstd chunk decode fixture size: {size}"),
    }
}

fn bench_crypto(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_crypto");
    let cipher =
        EcdhTeaCipher::new_with_private_key(SAMPLE_PUBKEY, [7u8; 32]).expect("cipher init");
    let key = cipher.tea_key_words();
    for size in [96usize, 256, 1024] {
        let aligned = (size / 8).max(1) * 8;
        let payload = make_binary_payload(aligned, 0x1234_5678);
        group.throughput(Throughput::Bytes(aligned as u64));

        group.bench_with_input(
            BenchmarkId::new("tea_encrypt_in_place", aligned),
            &aligned,
            |b, _| {
                b.iter_batched(
                    || payload.clone(),
                    |mut input| {
                        tea_encrypt_in_place(black_box(input.as_mut_slice()), black_box(&key));
                        black_box(input);
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("encrypt_async_in_place", aligned),
            &aligned,
            |b, _| {
                b.iter_batched(
                    || payload.clone(),
                    |mut input| {
                        cipher.encrypt_async_in_place(black_box(input.as_mut_slice()));
                        black_box(input);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = bench_formatter, bench_compression, bench_crypto
);
criterion_main!(benches);
