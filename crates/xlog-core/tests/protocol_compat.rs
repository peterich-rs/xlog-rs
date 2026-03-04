use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::SecretKey;

use mars_xlog_core::crypto::{tea_decrypt_in_place, EcdhTeaCipher};
use mars_xlog_core::protocol::{
    magic_start_is_valid, select_magic, update_end_hour_in_place, update_log_len_in_place,
    AppendMode, CompressionKind, LogHeader, SeqGenerator, HEADER_LEN,
};

#[test]
fn magic_selection_matches_cpp_constants() {
    assert_eq!(
        select_magic(CompressionKind::Zlib, AppendMode::Sync, true),
        0x06
    );
    assert_eq!(
        select_magic(CompressionKind::Zlib, AppendMode::Async, true),
        0x07
    );
    assert_eq!(
        select_magic(CompressionKind::Zlib, AppendMode::Sync, false),
        0x08
    );
    assert_eq!(
        select_magic(CompressionKind::Zlib, AppendMode::Async, false),
        0x09
    );
    assert_eq!(
        select_magic(CompressionKind::Zstd, AppendMode::Sync, true),
        0x0A
    );
    assert_eq!(
        select_magic(CompressionKind::Zstd, AppendMode::Sync, false),
        0x0B
    );
    assert_eq!(
        select_magic(CompressionKind::Zstd, AppendMode::Async, true),
        0x0C
    );
    assert_eq!(
        select_magic(CompressionKind::Zstd, AppendMode::Async, false),
        0x0D
    );

    for magic in 0x06u8..=0x0D {
        assert!(magic_start_is_valid(magic));
    }
    assert!(!magic_start_is_valid(0x05));
}

#[test]
fn header_roundtrip_and_mutation() {
    let mut pubkey = [0u8; 64];
    for (i, b) in pubkey.iter_mut().enumerate() {
        *b = i as u8;
    }

    let header = LogHeader {
        magic: 0x0C,
        seq: 7,
        begin_hour: 11,
        end_hour: 11,
        len: 1024,
        client_pubkey: pubkey,
    };

    let mut encoded = header.encode();
    assert_eq!(encoded.len(), HEADER_LEN);

    update_log_len_in_place(&mut encoded, 12).unwrap();
    update_end_hour_in_place(&mut encoded, 12).unwrap();

    let decoded = LogHeader::decode(&encoded).unwrap();
    assert_eq!(decoded.magic, 0x0C);
    assert_eq!(decoded.seq, 7);
    assert_eq!(decoded.begin_hour, 11);
    assert_eq!(decoded.end_hour, 12);
    assert_eq!(decoded.len, 1036);
    assert_eq!(decoded.client_pubkey, pubkey);
}

#[test]
fn async_seq_skips_zero() {
    let gen = SeqGenerator::with_seed(u16::MAX - 1);
    assert_eq!(gen.next_async(), u16::MAX);
    assert_eq!(gen.next_async(), 1);
    assert_eq!(gen.next_async(), 2);
    assert_eq!(SeqGenerator::sync_seq(), 0);
}

#[test]
fn ecdh_tea_derivation_matches_on_both_sides() {
    let a_priv = [1u8; 32];
    let b_priv = [2u8; 32];

    let b_secret = SecretKey::from_slice(&b_priv).unwrap();
    let b_point = b_secret.public_key().to_encoded_point(false);
    let b_pub_hex = hex::encode(&b_point.as_bytes()[1..65]);

    let a_secret = SecretKey::from_slice(&a_priv).unwrap();
    let a_point = a_secret.public_key().to_encoded_point(false);
    let a_pub_hex = hex::encode(&a_point.as_bytes()[1..65]);

    let a_cipher = EcdhTeaCipher::new_with_private_key(&b_pub_hex, a_priv).unwrap();
    let b_cipher = EcdhTeaCipher::new_with_private_key(&a_pub_hex, b_priv).unwrap();

    assert!(a_cipher.enabled());
    assert!(b_cipher.enabled());
    assert_eq!(a_cipher.tea_key_words(), b_cipher.tea_key_words());

    let plain = b"abcdefghijklmnopqrstuvwxyz";
    let encrypted = a_cipher.encrypt_async(plain);
    assert_eq!(&encrypted[24..], &plain[24..]);

    let mut decrypted = encrypted.clone();
    let block_end = decrypted.len() / 8 * 8;
    tea_decrypt_in_place(&mut decrypted[..block_end], &b_cipher.tea_key_words());
    assert_eq!(&decrypted[..], plain);

    let sync_out = a_cipher.encrypt_sync(plain);
    assert_eq!(&sync_out[..], plain);
}
