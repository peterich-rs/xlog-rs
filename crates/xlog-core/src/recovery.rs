use chrono::{Local, Timelike};

use crate::platform_tid::current_tid;
use crate::protocol::{
    AppendMode, BlockMagic, CompressionKind, Hour, LogHeader, HEADER_LEN, MAGIC_END,
};

pub(crate) fn magic_profile(magic: u8) -> Option<(CompressionKind, bool)> {
    let magic = BlockMagic::try_from(magic).ok()?;
    Some((magic.family(), magic.encrypted()))
}

pub(crate) fn build_sync_tip_block(sample_header: Option<LogHeader>, tip: &str) -> Option<Vec<u8>> {
    let sample = sample_header?;
    let (compression, crypt) = magic_profile(u8::from(sample.magic))?;
    let payload = tip.as_bytes();
    let now_hour = Hour::try_from(Local::now().hour() as u8).ok()?;
    let header = LogHeader {
        magic: BlockMagic::new(compression, AppendMode::Sync, crypt),
        seq: 0,
        begin_hour: now_hour,
        end_hour: now_hour,
        len: u32::try_from(payload.len()).ok()?,
        client_pubkey: if crypt { sample.client_pubkey } else { [0; 64] },
    };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + 1);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.push(MAGIC_END);
    Some(out)
}

pub(crate) fn current_mark_info() -> String {
    let now = Local::now();
    format!(
        "[{},{}][{}]",
        std::process::id(),
        current_tid(),
        now.format("%Y-%m-%d %z %H:%M:%S")
    )
}

#[cfg(test)]
mod tests {
    use super::{build_sync_tip_block, magic_profile};
    use crate::protocol::{
        AppendMode, BlockMagic, CompressionKind, Hour, LogHeader, HEADER_LEN,
        MAGIC_ASYNC_NO_CRYPT_ZLIB_START, MAGIC_ASYNC_NO_CRYPT_ZSTD_START, MAGIC_ASYNC_ZLIB_START,
        MAGIC_ASYNC_ZSTD_START, MAGIC_END, MAGIC_SYNC_NO_CRYPT_ZLIB_START,
        MAGIC_SYNC_NO_CRYPT_ZSTD_START, MAGIC_SYNC_ZLIB_START, MAGIC_SYNC_ZSTD_START,
    };

    #[test]
    fn magic_profile_maps_all_supported_magic_variants() {
        assert_eq!(
            magic_profile(MAGIC_SYNC_ZLIB_START),
            Some((CompressionKind::Zlib, true))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_ZLIB_START),
            Some((CompressionKind::Zlib, true))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_NO_CRYPT_ZLIB_START),
            Some((CompressionKind::Zlib, false))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_NO_CRYPT_ZLIB_START),
            Some((CompressionKind::Zlib, false))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_ZSTD_START),
            Some((CompressionKind::Zstd, true))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_ZSTD_START),
            Some((CompressionKind::Zstd, true))
        );
        assert_eq!(
            magic_profile(MAGIC_SYNC_NO_CRYPT_ZSTD_START),
            Some((CompressionKind::Zstd, false))
        );
        assert_eq!(
            magic_profile(MAGIC_ASYNC_NO_CRYPT_ZSTD_START),
            Some((CompressionKind::Zstd, false))
        );
        assert_eq!(magic_profile(0), None);
    }

    #[test]
    fn build_sync_tip_block_preserves_crypto_profile_for_encrypted_headers() {
        let sample = LogHeader {
            magic: BlockMagic::new(CompressionKind::Zstd, AppendMode::Async, true),
            seq: 42,
            begin_hour: Hour::try_from(1).unwrap(),
            end_hour: Hour::try_from(1).unwrap(),
            len: 3,
            client_pubkey: [7; 64],
        };

        let block = build_sync_tip_block(Some(sample), "tip").unwrap();
        let header = LogHeader::decode(&block[..HEADER_LEN]).unwrap();

        assert_eq!(
            header.magic,
            BlockMagic::new(CompressionKind::Zstd, AppendMode::Sync, true)
        );
        assert_eq!(header.client_pubkey, [7; 64]);
        assert_eq!(&block[HEADER_LEN..block.len() - 1], b"tip");
        assert_eq!(block[block.len() - 1], MAGIC_END);
    }

    #[test]
    fn build_sync_tip_block_zeroes_pubkey_for_plaintext_headers() {
        let sample = LogHeader {
            magic: BlockMagic::new(CompressionKind::Zlib, AppendMode::Async, false),
            seq: 7,
            begin_hour: Hour::try_from(1).unwrap(),
            end_hour: Hour::try_from(1).unwrap(),
            len: 5,
            client_pubkey: [9; 64],
        };

        let block = build_sync_tip_block(Some(sample), "plain").unwrap();
        let header = LogHeader::decode(&block[..HEADER_LEN]).unwrap();

        assert_eq!(
            header.magic,
            BlockMagic::new(CompressionKind::Zlib, AppendMode::Sync, false)
        );
        assert_eq!(header.client_pubkey, [0; 64]);
    }

    #[test]
    fn build_sync_tip_block_rejects_missing_or_unknown_sample_headers() {
        assert!(build_sync_tip_block(None, "tip").is_none());
        assert!(magic_profile(0).is_none());
    }
}
