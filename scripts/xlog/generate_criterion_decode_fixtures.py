#!/usr/bin/env python3
"""Generate fixed decode fixtures for Criterion compression benchmarks."""

from __future__ import annotations

import hashlib
import json
import subprocess
import zlib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / 'crates/xlog-core/benches/fixtures'
SIZES = (256, 1024, 4096)
SEED = 0xCAFE_BABE
ZSTD_LEVEL = 3
ZLIB_LEVEL = 6
ZSTD_BIN = 'zstd'


class XorShift64:
    def __init__(self, seed: int) -> None:
        self.state = seed if seed != 0 else 0x9E37_79B9_7F4A_7C15

    def next_u64(self) -> int:
        x = self.state & 0xFFFF_FFFF_FFFF_FFFF
        x ^= (x << 13) & 0xFFFF_FFFF_FFFF_FFFF
        x ^= x >> 7
        x ^= (x << 17) & 0xFFFF_FFFF_FFFF_FFFF
        self.state = x & 0xFFFF_FFFF_FFFF_FFFF
        return self.state


def make_binary_payload(length: int, seed: int) -> bytes:
    rng = XorShift64(seed ^ (length << 16))
    out = bytearray(length)
    for i in range(length):
        out[i] = rng.next_u64() & 0xFF
    return bytes(out)


def compress_zlib_raw(payload: bytes) -> bytes:
    obj = zlib.compressobj(level=ZLIB_LEVEL, method=zlib.DEFLATED, wbits=-zlib.MAX_WBITS)
    return obj.compress(payload) + obj.flush()


def compress_zstd_frame(payload: bytes) -> bytes:
    proc = subprocess.run(
        [ZSTD_BIN, '-q', f'-{ZSTD_LEVEL}', '--stdout', '--no-check'],
        input=payload,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    return proc.stdout


def compress_zstd_chunked(payload: bytes) -> bytes:
    chunk_len = max(1, len(payload) // 4)
    parts = []
    start = 0
    while start < len(payload):
        end = min(len(payload), start + chunk_len)
        parts.append(compress_zstd_frame(payload[start:end]))
        start = end
    return b''.join(parts)


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_fixture(name: str, payload: bytes) -> dict[str, object]:
    path = OUT_DIR / name
    path.write_bytes(payload)
    return {
        'file': path.name,
        'bytes': len(payload),
        'sha256': sha256_hex(payload),
    }


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    manifest: dict[str, object] = {
        'seed': SEED,
        'sizes': list(SIZES),
        'zlib_level': ZLIB_LEVEL,
        'zstd_level': ZSTD_LEVEL,
        'fixtures': [],
    }

    for size in SIZES:
        payload = make_binary_payload(size, SEED)
        manifest['fixtures'].append({
            'kind': 'raw_payload',
            'size': size,
            'sha256': sha256_hex(payload),
        })
        manifest['fixtures'].append({
            'kind': 'zlib_stream_l6',
            'size': size,
            **write_fixture(f'zlib_stream_l6_{size}.bin', compress_zlib_raw(payload)),
        })
        manifest['fixtures'].append({
            'kind': 'zstd_stream_l3',
            'size': size,
            **write_fixture(f'zstd_stream_l3_{size}.bin', compress_zstd_frame(payload)),
        })
        manifest['fixtures'].append({
            'kind': 'zstd_chunk_l3',
            'size': size,
            **write_fixture(f'zstd_chunk_l3_{size}.bin', compress_zstd_chunked(payload)),
        })

    (OUT_DIR / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    main()
