use crate::compress::{decompress_raw_zlib, decompress_zstd_frames, CompressError};
use crate::crypto::{tea_decrypt_in_place, CryptoError, EcdhTeaCipher};
use crate::protocol::{
    MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
    MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
    MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
};
use hex::FromHexError;
use thiserror::Error;

const BASE_KEY: u8 = 0xcc;
const LEGACY_MAGIC_CRYPT_START: u8 = 0x01;
const LEGACY_MAGIC_COMPRESS_CRYPT_START: u8 = 0x02;
const LEGACY_NEW_MAGIC_CRYPT_START: u8 = 0x03;
const LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START: u8 = 0x04;
const LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START1: u8 = 0x05;

/// Summary of a decoded xlog file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeSummary {
    /// Number of complete blocks consumed.
    pub blocks: usize,
    /// Number of payload bytes written after decode/decrypt/decompress.
    pub output_bytes: usize,
}

/// Options controlling xlog decode behavior.
#[derive(Debug, Clone, Default)]
pub struct DecodeOptions {
    private_key: Option<[u8; 32]>,
}

impl DecodeOptions {
    /// Creates decode options for plaintext xlog files.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures the 32-byte server private key used for encrypted async blocks.
    pub fn private_key(mut self, private_key: [u8; 32]) -> Self {
        self.private_key = Some(private_key);
        self
    }

    /// Parses and configures a 64-hex-character server private key.
    pub fn private_key_hex(self, private_key_hex: &str) -> Result<Self, DecodeError> {
        Ok(self.private_key(parse_private_key_hex(private_key_hex)?))
    }
}

/// Errors raised while decoding xlog files.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The private key was not a 64-character hex string.
    #[error("private key must be 64 hex chars")]
    InvalidPrivateKeyLength,
    /// The private key hex string could not be decoded.
    #[error("invalid private key hex: {0}")]
    InvalidPrivateKeyHex(#[from] FromHexError),
    /// The private key or header public key was invalid secp256k1 material.
    #[error("invalid secp256k1 key material")]
    InvalidKeyMaterial,
    /// The xlog input did not contain a decodable log block.
    #[error("no valid xlog block found")]
    NoValidBlock,
    /// An encrypted async block was found without a configured private key.
    #[error("encrypted xlog block requires --key/--private-key")]
    MissingPrivateKey,
    /// A log block declared a payload that extends beyond the input buffer.
    #[error("truncated xlog block at offset {offset}: payload len {len}")]
    TruncatedBlock {
        /// Offset where the broken block starts.
        offset: usize,
        /// Payload length declared by the block header.
        len: usize,
    },
    /// A log block was not followed by the expected end marker.
    #[error("invalid xlog tail marker at offset {offset}")]
    InvalidTail {
        /// Offset where the invalid tail byte was found.
        offset: usize,
    },
    /// Compression failed to decode.
    #[error(transparent)]
    Compress(#[from] CompressError),
    /// ECDH/TEA setup failed.
    #[error(transparent)]
    Crypto(CryptoError),
}

impl From<CryptoError> for DecodeError {
    fn from(value: CryptoError) -> Self {
        match value {
            CryptoError::InvalidKeyMaterial => Self::InvalidKeyMaterial,
            other => Self::Crypto(other),
        }
    }
}

#[derive(Debug, Clone)]
struct BlockHeader {
    magic: u8,
    header_len: usize,
    len: usize,
    seq: u16,
    client_pubkey: [u8; 64],
}

/// Decodes `input` into `output` using the supplied options.
pub fn decode_xlog(
    input: &[u8],
    output: &mut Vec<u8>,
    opts: &DecodeOptions,
) -> Result<DecodeSummary, DecodeError> {
    let Some(mut offset) = find_log_start(input, 2) else {
        return Err(DecodeError::NoValidBlock);
    };

    let mut blocks = 0usize;
    let mut last_seq = 0u16;

    loop {
        if offset >= input.len() {
            break;
        }

        if !is_good_log_buffer(input, offset, 1) {
            let Some(fix_pos) = find_log_start(&input[offset..], 1) else {
                break;
            };
            output.extend_from_slice(
                format!("[F]decode_log_file.py decode error len={fix_pos}\n").as_bytes(),
            );
            offset += fix_pos;
        }

        let header = parse_header(input, offset)?;
        let payload_start = offset + header.header_len;
        let payload_end = payload_start + header.len;
        if payload_end + 1 > input.len() {
            return Err(DecodeError::TruncatedBlock {
                offset,
                len: header.len,
            });
        }
        if input[payload_end] != MAGIC_END {
            return Err(DecodeError::InvalidTail {
                offset: payload_end,
            });
        }

        if header.seq != 0
            && header.seq != 1
            && last_seq != 0
            && header.seq != last_seq.wrapping_add(1)
        {
            output.extend_from_slice(
                format!(
                    "[F]decode_log_file.py log seq:{}-{} is missing\n",
                    last_seq.wrapping_add(1),
                    header.seq.wrapping_sub(1)
                )
                .as_bytes(),
            );
        }
        if header.seq != 0 {
            last_seq = header.seq;
        }

        let payload = &input[payload_start..payload_end];
        let decoded = decode_payload(&header, payload, opts)?;
        output.extend_from_slice(&decoded);
        blocks += 1;
        offset = payload_end + 1;
    }

    if blocks == 0 {
        return Err(DecodeError::NoValidBlock);
    }

    Ok(DecodeSummary {
        blocks,
        output_bytes: output.len(),
    })
}

/// Parses a 64-hex-character private key into raw bytes.
pub fn parse_private_key_hex(private_key_hex: &str) -> Result<[u8; 32], DecodeError> {
    let trimmed = private_key_hex.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if trimmed.len() != 64 {
        return Err(DecodeError::InvalidPrivateKeyLength);
    }

    let mut out = [0u8; 32];
    hex::decode_to_slice(trimmed, &mut out)?;
    Ok(out)
}

fn decode_payload(
    header: &BlockHeader,
    payload: &[u8],
    opts: &DecodeOptions,
) -> Result<Vec<u8>, DecodeError> {
    match header.magic {
        LEGACY_MAGIC_CRYPT_START | LEGACY_NEW_MAGIC_CRYPT_START => {
            Ok(xor_payload(payload, legacy_xor_key(header)))
        }
        LEGACY_MAGIC_COMPRESS_CRYPT_START | LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START => {
            let decoded = xor_payload(payload, legacy_xor_key(header));
            Ok(decompress_raw_zlib(&decoded)?)
        }
        LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START1 => {
            let joined = join_legacy_compressed_segments(payload);
            let decoded = xor_payload(&joined, legacy_xor_key(header));
            Ok(decompress_raw_zlib(&decoded)?)
        }
        MAGIC_SYNC_ZLIB_START
        | MAGIC_SYNC_NO_CRYPT_ZLIB_START
        | MAGIC_SYNC_ZSTD_START
        | MAGIC_SYNC_NO_CRYPT_ZSTD_START => Ok(payload.to_vec()),
        MAGIC_ASYNC_ZLIB_START | MAGIC_ASYNC_ZSTD_START => {
            let private_key = opts.private_key.ok_or(DecodeError::MissingPrivateKey)?;
            let cipher =
                EcdhTeaCipher::new_with_server_private_key(header.client_pubkey, private_key)?;
            let mut decoded = payload.to_vec();
            let block_end = decoded.len() / 8 * 8;
            tea_decrypt_in_place(&mut decoded[..block_end], &cipher.tea_key_words());
            match header.magic {
                MAGIC_ASYNC_ZLIB_START => Ok(decompress_raw_zlib(&decoded)?),
                MAGIC_ASYNC_ZSTD_START => Ok(decompress_zstd_frames(&decoded)?),
                _ => unreachable!(),
            }
        }
        MAGIC_ASYNC_NO_CRYPT_ZLIB_START => Ok(decompress_raw_zlib(payload)?),
        MAGIC_ASYNC_NO_CRYPT_ZSTD_START => Ok(decompress_zstd_frames(payload)?),
        _ => Err(DecodeError::NoValidBlock),
    }
}

fn parse_header(input: &[u8], offset: usize) -> Result<BlockHeader, DecodeError> {
    let magic = input
        .get(offset)
        .copied()
        .ok_or(DecodeError::NoValidBlock)?;
    let Some((header_len, crypt_key_len)) = header_lengths_for_magic(magic) else {
        return Err(DecodeError::NoValidBlock);
    };
    if offset + header_len > input.len() {
        return Err(DecodeError::TruncatedBlock { offset, len: 0 });
    }

    let len_offset = offset + header_len - crypt_key_len - 4;
    let len = u32::from_le_bytes([
        input[len_offset],
        input[len_offset + 1],
        input[len_offset + 2],
        input[len_offset + 3],
    ]) as usize;

    let seq = if header_len >= 1 + 2 + 1 + 1 + 4 {
        let seq_offset = offset + header_len - crypt_key_len - 4 - 2 - 2;
        u16::from_le_bytes([input[seq_offset], input[seq_offset + 1]])
    } else {
        0
    };

    let mut client_pubkey = [0u8; 64];
    if crypt_key_len == 64 {
        let key_start = offset + header_len - crypt_key_len;
        client_pubkey.copy_from_slice(&input[key_start..key_start + 64]);
    }

    Ok(BlockHeader {
        magic,
        header_len,
        len,
        seq,
        client_pubkey,
    })
}

fn header_lengths_for_magic(magic: u8) -> Option<(usize, usize)> {
    match magic {
        LEGACY_MAGIC_CRYPT_START | LEGACY_MAGIC_COMPRESS_CRYPT_START => Some((1 + 4, 0)),
        LEGACY_NEW_MAGIC_CRYPT_START
        | LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START
        | LEGACY_NEW_MAGIC_COMPRESS_CRYPT_START1 => Some((1 + 2 + 1 + 1 + 4, 0)),
        MAGIC_SYNC_ZLIB_START
        | MAGIC_SYNC_NO_CRYPT_ZLIB_START
        | MAGIC_ASYNC_ZLIB_START
        | MAGIC_ASYNC_NO_CRYPT_ZLIB_START
        | MAGIC_SYNC_ZSTD_START
        | MAGIC_SYNC_NO_CRYPT_ZSTD_START
        | MAGIC_ASYNC_ZSTD_START
        | MAGIC_ASYNC_NO_CRYPT_ZSTD_START => Some((1 + 2 + 1 + 1 + 4 + 64, 64)),
        _ => None,
    }
}

fn is_good_log_buffer(input: &[u8], offset: usize, count: usize) -> bool {
    if offset == input.len() {
        return true;
    }
    if count == 0 {
        return true;
    }
    let Ok(header) = parse_header(input, offset) else {
        return false;
    };
    let Some(payload_end) = offset
        .checked_add(header.header_len)
        .and_then(|value| value.checked_add(header.len))
    else {
        return false;
    };
    if payload_end + 1 > input.len() || input[payload_end] != MAGIC_END {
        return false;
    }
    if count <= 1 {
        return true;
    }
    is_good_log_buffer(input, payload_end + 1, count - 1)
}

fn find_log_start(input: &[u8], count: usize) -> Option<usize> {
    let mut offset = 0usize;
    while offset < input.len() {
        if header_lengths_for_magic(input[offset]).is_some()
            && is_good_log_buffer(input, offset, count)
        {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

fn legacy_xor_key(header: &BlockHeader) -> u8 {
    match header.magic {
        LEGACY_MAGIC_CRYPT_START | LEGACY_MAGIC_COMPRESS_CRYPT_START => {
            BASE_KEY ^ (header.len as u8) ^ header.magic
        }
        _ => BASE_KEY ^ (header.seq as u8) ^ header.magic,
    }
}

fn xor_payload(payload: &[u8], key: u8) -> Vec<u8> {
    payload.iter().map(|byte| byte ^ key).collect()
}

fn join_legacy_compressed_segments(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut offset = 0usize;
    while offset + 2 <= payload.len() {
        let len = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
        offset += 2;
        let end = offset.saturating_add(len).min(payload.len());
        out.extend_from_slice(&payload[offset..end]);
        offset = end;
    }
    out
}
