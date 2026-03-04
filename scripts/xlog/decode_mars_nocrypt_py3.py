#!/usr/bin/env python3
"""
Python3-compatible fallback decoder for non-crypt Mars xlog files.

It follows the same buffer layout and parsing strategy as
third_party/mars/mars/xlog/crypt/decode_mars_nocrypt_log_file.py, but avoids
Python2-only syntax and third-party Python modules.
"""

import glob
import os
import struct
import subprocess
import sys
import traceback
import zlib

MAGIC_NO_COMPRESS_START = 0x03
MAGIC_NO_COMPRESS_START1 = 0x06
MAGIC_NO_COMPRESS_NO_CRYPT_START = 0x08
MAGIC_COMPRESS_START = 0x04
MAGIC_COMPRESS_START1 = 0x05
MAGIC_COMPRESS_START2 = 0x07
MAGIC_COMPRESS_NO_CRYPT_START = 0x09

MAGIC_SYNC_ZSTD_START = 0x0A
MAGIC_SYNC_NO_CRYPT_ZSTD_START = 0x0B
MAGIC_ASYNC_ZSTD_START = 0x0C
MAGIC_ASYNC_NO_CRYPT_ZSTD_START = 0x0D

MAGIC_END = 0x00

lastseq = 0


def is_valid_magic(value):
    return value in {
        MAGIC_NO_COMPRESS_START,
        MAGIC_NO_COMPRESS_START1,
        MAGIC_NO_COMPRESS_NO_CRYPT_START,
        MAGIC_COMPRESS_START,
        MAGIC_COMPRESS_START1,
        MAGIC_COMPRESS_START2,
        MAGIC_COMPRESS_NO_CRYPT_START,
        MAGIC_SYNC_ZSTD_START,
        MAGIC_SYNC_NO_CRYPT_ZSTD_START,
        MAGIC_ASYNC_ZSTD_START,
        MAGIC_ASYNC_NO_CRYPT_ZSTD_START,
    }


def crypt_key_len_for_magic(value):
    if value in (MAGIC_NO_COMPRESS_START, MAGIC_COMPRESS_START, MAGIC_COMPRESS_START1):
        return 4
    if is_valid_magic(value):
        return 64
    return -1


def is_good_log_buffer(buf, offset, count):
    if offset == len(buf):
        return (True, "")

    magic_start = buf[offset]
    crypt_key_len = crypt_key_len_for_magic(magic_start)
    if crypt_key_len < 0:
        return (False, f"_buffer[{offset}]:{buf[offset]} != MAGIC_NUM_START")

    header_len = 1 + 2 + 1 + 1 + 4 + crypt_key_len
    if offset + header_len + 1 + 1 > len(buf):
        return (False, f"offset:{offset} > len(buffer):{len(buf)}")

    length = struct.unpack_from("<I", buf, offset + header_len - 4 - crypt_key_len)[0]
    end_pos = offset + header_len + length
    if end_pos + 1 > len(buf):
        return (
            False,
            f"log length:{length}, end pos {end_pos + 1} > len(buffer):{len(buf)}",
        )
    if MAGIC_END != buf[end_pos]:
        return (
            False,
            f"log length:{length}, buffer[{end_pos}]:{buf[end_pos]} != MAGIC_END",
        )

    if count <= 1:
        return (True, "")
    return is_good_log_buffer(buf, end_pos + 1, count - 1)


def get_log_start_pos(buf, count):
    offset = 0
    while offset < len(buf):
        if is_valid_magic(buf[offset]):
            good, _ = is_good_log_buffer(buf, offset, count)
            if good:
                return offset
        offset += 1
    return -1


def decode_zstd(compressed):
    proc = subprocess.run(
        ["zstd", "-d", "-q", "--stdout"],
        input=compressed,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        # Async blocks may carry a flushed-but-not-ended stream chunk.
        # Keep partial output to match the permissive behavior of the legacy decoder.
        if proc.stdout:
            return proc.stdout
        raise RuntimeError(proc.stderr.decode("utf-8", errors="replace"))
    return proc.stdout


def decode_zlib_partial(compressed):
    decompressor = zlib.decompressobj(-zlib.MAX_WBITS)
    return decompressor.decompress(compressed)


def decode_buffer(buf, offset, out):
    global lastseq

    if offset >= len(buf):
        return -1

    good, reason = is_good_log_buffer(buf, offset, 1)
    if not good:
        fixpos = get_log_start_pos(buf[offset:], 1)
        if fixpos == -1:
            return -1
        out.extend(
            f"[F]decode_log_file.py decode error len={fixpos}, result:{reason}\n".encode(
                "utf-8"
            )
        )
        offset += fixpos

    magic_start = buf[offset]
    crypt_key_len = crypt_key_len_for_magic(magic_start)
    if crypt_key_len < 0:
        out.extend(
            f"in DecodeBuffer _buffer[{offset}]:{magic_start} != MAGIC_NUM_START".encode(
                "utf-8"
            )
        )
        return -1

    header_len = 1 + 2 + 1 + 1 + 4 + crypt_key_len
    length = struct.unpack_from("<I", buf, offset + header_len - 4 - crypt_key_len)[0]

    seq = struct.unpack_from("<H", buf, offset + header_len - 4 - crypt_key_len - 2 - 2)[0]
    if seq != 0 and seq != 1 and lastseq != 0 and seq != (lastseq + 1):
        out.extend(
            f"[F]decode_log_file.py log seq:{lastseq + 1}-{seq - 1} is missing\n".encode(
                "utf-8"
            )
        )
    if seq != 0:
        lastseq = seq

    payload = bytes(buf[offset + header_len : offset + header_len + length])

    try:
        if magic_start in (
            MAGIC_NO_COMPRESS_START1,
            MAGIC_COMPRESS_START2,
            MAGIC_SYNC_ZSTD_START,
            MAGIC_ASYNC_ZSTD_START,
        ):
            raise RuntimeError("use wrong decode script")
        if magic_start == MAGIC_ASYNC_NO_CRYPT_ZSTD_START:
            payload = decode_zstd(payload)
        elif magic_start in (MAGIC_COMPRESS_START, MAGIC_COMPRESS_NO_CRYPT_START):
            payload = decode_zlib_partial(payload)
        elif magic_start == MAGIC_COMPRESS_START1:
            decompress_data = bytearray()
            tmp = payload
            while tmp:
                single_log_len = struct.unpack_from("<H", tmp, 0)[0]
                start = 2
                end = single_log_len + 2
                decompress_data.extend(tmp[start:end])
                tmp = tmp[end:]
            payload = decode_zlib_partial(bytes(decompress_data))
    except Exception as e:  # pragma: no cover - best effort parity with official script
        traceback.print_exc()
        out.extend(f"[F]decode_log_file.py decompress err, {e}\n".encode("utf-8"))
        return offset + header_len + length + 1

    out.extend(payload)
    return offset + header_len + length + 1


def parse_file(input_path, output_path):
    with open(input_path, "rb") as fp:
        buf = bytearray(fp.read())

    startpos = get_log_start_pos(buf, 2)
    if startpos == -1:
        return

    out = bytearray()
    while True:
        startpos = decode_buffer(buf, startpos, out)
        if startpos == -1:
            break

    if not out:
        return

    with open(output_path, "wb") as out_fp:
        out_fp.write(out)


def main(argv):
    global lastseq

    if len(argv) == 1:
        arg0 = argv[0]
        if os.path.isdir(arg0):
            for filepath in glob.glob(os.path.join(arg0, "*.xlog")):
                lastseq = 0
                parse_file(filepath, filepath + ".log")
        else:
            parse_file(arg0, arg0 + ".log")
        return 0

    if len(argv) == 2:
        parse_file(argv[0], argv[1])
        return 0

    for filepath in glob.glob("*.xlog"):
        lastseq = 0
        parse_file(filepath, filepath + ".log")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
