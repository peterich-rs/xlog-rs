use mars_xlog_core::compress::{StreamCompressor, ZlibStreamCompressor, ZstdStreamCompressor};
use mars_xlog_core::crypto::EcdhTeaCipher;
use mars_xlog_core::decoder::{decode_xlog, DecodeError, DecodeOptions};
use mars_xlog_core::protocol::{
    select_magic, AppendMode, CompressionKind, LogHeader, MAGIC_ASYNC_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZLIB_START,
};

const TEST_SERVER_PUBKEY_HEX: &str = concat!(
    "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
    "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8"
);
const TEST_SERVER_PRIVKEY_ONE: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];

fn block(magic: u8, seq: u16, client_pubkey: [u8; 64], payload: &[u8]) -> Vec<u8> {
    let header = LogHeader {
        magic,
        seq,
        begin_hour: 1,
        end_hour: 1,
        len: payload.len() as u32,
        client_pubkey,
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(payload);
    bytes.push(0);
    bytes
}

#[test]
fn decodes_sync_plaintext_block() {
    let bytes = block(MAGIC_SYNC_NO_CRYPT_ZLIB_START, 0, [0; 64], b"sync line\n");

    let mut out = Vec::new();
    let summary = decode_xlog(&bytes, &mut out, &DecodeOptions::new()).unwrap();

    assert_eq!(summary.blocks, 1);
    assert_eq!(out, b"sync line\n");
}

#[test]
fn decodes_async_zlib_and_zstd_plaintext_blocks() {
    let mut zlib = ZlibStreamCompressor::new(6);
    let mut zlib_payload = Vec::new();
    zlib.compress_chunk(b"zlib one\n", &mut zlib_payload)
        .unwrap();
    zlib.flush(&mut zlib_payload).unwrap();

    let mut zstd = ZstdStreamCompressor::new(3).unwrap();
    let mut zstd_payload = Vec::new();
    zstd.compress_chunk(b"zstd two\n", &mut zstd_payload)
        .unwrap();
    zstd.flush(&mut zstd_payload).unwrap();

    let mut bytes = Vec::new();
    bytes.extend(block(
        select_magic(CompressionKind::Zlib, AppendMode::Async, false),
        1,
        [0; 64],
        &zlib_payload,
    ));
    bytes.extend(block(
        select_magic(CompressionKind::Zstd, AppendMode::Async, false),
        2,
        [0; 64],
        &zstd_payload,
    ));

    let mut out = Vec::new();
    let summary = decode_xlog(&bytes, &mut out, &DecodeOptions::new()).unwrap();

    assert_eq!(summary.blocks, 2);
    assert_eq!(out, b"zlib one\nzstd two\n");
}

#[test]
fn decodes_async_encrypted_zlib_block_with_private_key() {
    let cipher = EcdhTeaCipher::new_with_private_key(TEST_SERVER_PUBKEY_HEX, [7; 32]).unwrap();
    let mut compressor = ZlibStreamCompressor::new(6);
    let mut payload = Vec::new();
    compressor
        .compress_chunk(b"secret async line\n", &mut payload)
        .unwrap();
    compressor.flush(&mut payload).unwrap();
    let encrypted = cipher.encrypt_async(&payload);

    let bytes = block(
        MAGIC_ASYNC_ZLIB_START,
        1,
        cipher.client_pubkey(),
        &encrypted,
    );

    let opts = DecodeOptions::new().private_key(TEST_SERVER_PRIVKEY_ONE);
    let mut out = Vec::new();
    let summary = decode_xlog(&bytes, &mut out, &opts).unwrap();

    assert_eq!(summary.blocks, 1);
    assert_eq!(out, b"secret async line\n");
}

#[test]
fn encrypted_async_block_requires_private_key() {
    let cipher = EcdhTeaCipher::new_with_private_key(TEST_SERVER_PUBKEY_HEX, [9; 32]).unwrap();
    let bytes = block(
        MAGIC_ASYNC_ZLIB_START,
        1,
        cipher.client_pubkey(),
        b"payload",
    );

    let mut out = Vec::new();
    let err = decode_xlog(&bytes, &mut out, &DecodeOptions::new()).unwrap_err();

    assert!(matches!(err, DecodeError::MissingPrivateKey));
}

#[test]
fn skips_prefix_bytes_before_first_block() {
    let mut bytes = b"noise".to_vec();
    bytes.extend(block(
        MAGIC_SYNC_NO_CRYPT_ZLIB_START,
        0,
        [0; 64],
        b"after noise\n",
    ));

    let mut out = Vec::new();
    decode_xlog(&bytes, &mut out, &DecodeOptions::new()).unwrap();

    assert_eq!(out, b"after noise\n");
}
