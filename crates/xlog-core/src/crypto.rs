use std::fmt;
use std::str::FromStr;

use hex::FromHexError;
use k256::ecdh::diffie_hellman;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::{PublicKey, SecretKey};
use rand_core::OsRng;
use thiserror::Error;

const TEA_BLOCK_LEN: usize = 8;

#[derive(Debug, Error)]
/// Errors raised while preparing or using the ECDH + TEA compatibility cipher.
pub enum CryptoError {
    /// The configured server public key was not 64 bytes of uncompressed hex data.
    #[error("server public key must be 128 hex chars")]
    InvalidServerPubkeyLength,
    /// Decoding the configured server public key hex failed.
    #[error("invalid server public key hex")]
    InvalidServerPubkeyHex(#[source] FromHexError),
    /// The private key was not 32 bytes of hex.
    #[error("private key must be 64 hex chars")]
    InvalidPrivateKeyLength,
    /// Decoding the private key hex failed.
    #[error("invalid private key hex")]
    InvalidPrivateKeyHex(#[source] FromHexError),
    /// The provided key bytes were not valid secp256k1 key material.
    #[error("invalid secp256k1 key material")]
    InvalidKeyMaterial,
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// 64-byte uncompressed secp256k1 public key, without the `0x04` prefix.
pub struct ServerPublicKey([u8; 64]);

impl fmt::Debug for ServerPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ServerPublicKey(..)")
    }
}

impl FromStr for ServerPublicKey {
    type Err = CryptoError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 128 {
            return Err(CryptoError::InvalidServerPubkeyLength);
        }
        let mut raw = [0u8; 64];
        hex::decode_to_slice(value, &mut raw).map_err(CryptoError::InvalidServerPubkeyHex)?;
        decode_uncompressed_pubkey(&raw)?;
        Ok(Self(raw))
    }
}

impl AsRef<[u8]> for ServerPublicKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// 32-byte secp256k1 private key.
pub struct PrivateKey([u8; 32]);

impl fmt::Debug for PrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateKey(..)")
    }
}

impl PrivateKey {
    /// Return the raw key bytes.
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

impl FromStr for PrivateKey {
    type Err = CryptoError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim().strip_prefix("0x").unwrap_or(value.trim());
        if trimmed.len() != 64 {
            return Err(CryptoError::InvalidPrivateKeyLength);
        }
        let mut raw = [0u8; 32];
        hex::decode_to_slice(trimmed, &mut raw).map_err(CryptoError::InvalidPrivateKeyHex)?;
        SecretKey::from_slice(&raw).map_err(|_| CryptoError::InvalidKeyMaterial)?;
        Ok(Self(raw))
    }
}

impl TryFrom<[u8; 32]> for PrivateKey {
    type Error = CryptoError;

    fn try_from(value: [u8; 32]) -> Result<Self, Self::Error> {
        SecretKey::from_slice(&value).map_err(|_| CryptoError::InvalidKeyMaterial)?;
        Ok(Self(value))
    }
}

impl From<PrivateKey> for [u8; 32] {
    fn from(value: PrivateKey) -> Self {
        value.0
    }
}

/// Async payload cipher. Sync frames never transform payload bytes.
#[derive(Clone, Copy, Default)]
pub enum Crypt {
    /// Header key slot stays zero and payload bytes are copied unchanged.
    #[default]
    Off,
    /// TEA key derived from secp256k1 ECDH. `Debug` omits the key.
    Tea {
        key: [u32; 4],
        client_pubkey: [u8; 64],
    },
}

impl fmt::Debug for Crypt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => formatter.write_str("Crypt::Off"),
            Self::Tea { client_pubkey, .. } => formatter
                .debug_struct("Crypt::Tea")
                .field("client_pubkey", client_pubkey)
                .finish_non_exhaustive(),
        }
    }
}

impl Crypt {
    /// No encryption.
    pub const fn off() -> Self {
        Self::Off
    }

    /// Opens a client cipher from a server public key.
    ///
    /// An empty string is the historical "encryption disabled" input and returns
    /// [`Crypt::Off`]. Any other string is parsed and rejected when it is not a
    /// valid key.
    pub fn open(server_pubkey_hex: &str) -> Result<Self, CryptoError> {
        if server_pubkey_hex.is_empty() {
            return Ok(Self::Off);
        }
        let server = server_pubkey_hex.parse::<ServerPublicKey>()?;
        let secret = SecretKey::random(&mut OsRng);
        Self::from_secret(server, secret)
    }

    /// Opens a client cipher from a caller-supplied private key.
    pub fn from_client_secret(
        server_pubkey_hex: &str,
        private_key: PrivateKey,
    ) -> Result<Self, CryptoError> {
        let server = server_pubkey_hex.parse::<ServerPublicKey>()?;
        let secret = SecretKey::from_slice(&private_key.bytes())
            .map_err(|_| CryptoError::InvalidKeyMaterial)?;
        Self::from_secret(server, secret)
    }

    /// Opens the server-side cipher used to decrypt an async block.
    pub fn from_server_secret(
        client_pubkey: [u8; 64],
        server_private_key: PrivateKey,
    ) -> Result<Self, CryptoError> {
        let secret = SecretKey::from_slice(&server_private_key.bytes())
            .map_err(|_| CryptoError::InvalidKeyMaterial)?;
        let client_public_key = decode_uncompressed_pubkey(&client_pubkey)?;
        let shared = diffie_hellman(secret.to_nonzero_scalar(), client_public_key.as_affine());
        Ok(Self::Tea {
            key: tea_words(shared.raw_secret_bytes().as_ref()),
            client_pubkey,
        })
    }

    /// Returns the client public key placed in the xlog header.
    pub fn client_pubkey(&self) -> [u8; 64] {
        match self {
            Self::Off => [0; 64],
            Self::Tea { client_pubkey, .. } => *client_pubkey,
        }
    }

    /// Returns the TEA key when encryption is enabled.
    pub fn tea_key(&self) -> Option<[u32; 4]> {
        match self {
            Self::Off => None,
            Self::Tea { key, .. } => Some(*key),
        }
    }

    /// Encrypts whole 8-byte blocks in place. [`Crypt::Off`] leaves `input` unchanged.
    pub fn encrypt_async_in_place(&self, input: &mut [u8]) {
        let Self::Tea { key, .. } = self else {
            return;
        };
        let block_bytes = input.len() / TEA_BLOCK_LEN * TEA_BLOCK_LEN;
        tea_encrypt_in_place(&mut input[..block_bytes], key);
    }

    fn from_secret(server: ServerPublicKey, secret: SecretKey) -> Result<Self, CryptoError> {
        let server_pubkey = decode_uncompressed_pubkey(server.as_ref())?;
        let shared = diffie_hellman(secret.to_nonzero_scalar(), server_pubkey.as_affine());
        let point = secret.public_key().to_encoded_point(false);
        let point_bytes = point.as_bytes();
        if point_bytes.len() != 65 {
            return Err(CryptoError::InvalidKeyMaterial);
        }
        let mut client_pubkey = [0u8; 64];
        client_pubkey.copy_from_slice(&point_bytes[1..65]);
        Ok(Self::Tea {
            key: tea_words(shared.raw_secret_bytes().as_ref()),
            client_pubkey,
        })
    }
}

fn tea_words(shared_bytes: &[u8]) -> [u32; 4] {
    let mut tea_key = [0u32; 4];
    for (index, word) in tea_key.iter_mut().enumerate() {
        let start = index * 4;
        *word = u32::from_le_bytes([
            shared_bytes[start],
            shared_bytes[start + 1],
            shared_bytes[start + 2],
            shared_bytes[start + 3],
        ]);
    }
    tea_key
}

fn decode_uncompressed_pubkey(raw: &[u8]) -> Result<PublicKey, CryptoError> {
    if raw.len() != 64 {
        return Err(CryptoError::InvalidServerPubkeyLength);
    }
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(raw);
    PublicKey::from_sec1_bytes(&sec1).map_err(|_| CryptoError::InvalidKeyMaterial)
}

/// Encrypts data in-place with the TEA block cipher using 16 rounds.
pub fn tea_encrypt_in_place(data: &mut [u8], key: &[u32; 4]) {
    for chunk in data.chunks_exact_mut(TEA_BLOCK_LEN) {
        let mut v0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let mut v1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);

        let mut sum = 0u32;
        let delta = 0x9e37_79b9u32;
        for _ in 0..16 {
            sum = sum.wrapping_add(delta);
            v0 = v0.wrapping_add(
                ((v1 << 4).wrapping_add(key[0]))
                    ^ (v1.wrapping_add(sum))
                    ^ ((v1 >> 5).wrapping_add(key[1])),
            );
            v1 = v1.wrapping_add(
                ((v0 << 4).wrapping_add(key[2]))
                    ^ (v0.wrapping_add(sum))
                    ^ ((v0 >> 5).wrapping_add(key[3])),
            );
        }

        chunk[..4].copy_from_slice(&v0.to_le_bytes());
        chunk[4..8].copy_from_slice(&v1.to_le_bytes());
    }
}

/// Decrypts data in-place with the TEA block cipher using 16 rounds.
pub fn tea_decrypt_in_place(data: &mut [u8], key: &[u32; 4]) {
    for chunk in data.chunks_exact_mut(TEA_BLOCK_LEN) {
        let mut v0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let mut v1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);

        let delta = 0x9e37_79b9u32;
        let mut sum = delta << 4;
        for _ in 0..16 {
            v1 = v1.wrapping_sub(
                ((v0 << 4).wrapping_add(key[2]))
                    ^ (v0.wrapping_add(sum))
                    ^ ((v0 >> 5).wrapping_add(key[3])),
            );
            v0 = v0.wrapping_sub(
                ((v1 << 4).wrapping_add(key[0]))
                    ^ (v1.wrapping_add(sum))
                    ^ ((v1 >> 5).wrapping_add(key[1])),
            );
            sum = sum.wrapping_sub(delta);
        }

        chunk[..4].copy_from_slice(&v0.to_le_bytes());
        chunk[4..8].copy_from_slice(&v1.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{tea_decrypt_in_place, Crypt, CryptoError, PrivateKey};

    const SAMPLE_PUBKEY: &str =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8";

    #[test]
    fn disabled_cipher_is_a_noop() {
        let cipher = Crypt::off();
        let plain = b"hello world";
        let mut in_place = plain.to_vec();

        assert!(cipher.tea_key().is_none());
        assert_eq!(cipher.client_pubkey(), [0; 64]);
        cipher.encrypt_async_in_place(&mut in_place);
        assert_eq!(in_place, plain);
        assert!(matches!(Crypt::open("").unwrap(), Crypt::Off));
    }

    #[test]
    fn constructor_rejects_invalid_key_material() {
        let private_key = PrivateKey::from_str_unchecked_for_test([7u8; 32]);
        assert!(matches!(
            Crypt::from_client_secret("abcd", private_key),
            Err(CryptoError::InvalidServerPubkeyLength)
        ));
        assert!(matches!(
            Crypt::from_client_secret(&"z".repeat(128), private_key),
            Err(CryptoError::InvalidServerPubkeyHex(_))
        ));
        assert!(matches!(
            Crypt::from_client_secret(&"00".repeat(64), private_key),
            Err(CryptoError::InvalidKeyMaterial)
        ));
        let zero = PrivateKey::from_str_unchecked_for_test([0u8; 32]);
        assert!(matches!(
            Crypt::from_client_secret(SAMPLE_PUBKEY, zero),
            Err(CryptoError::InvalidKeyMaterial)
        ));
        assert!(PrivateKey::from_str_result("abcd").is_err());
    }

    #[test]
    fn async_encryption_preserves_partial_tail_bytes() {
        let private_key = PrivateKey::from_str_unchecked_for_test([7u8; 32]);
        let cipher = Crypt::from_client_secret(SAMPLE_PUBKEY, private_key).unwrap();
        let original = b"abcdefghXYZ".to_vec();
        let mut encrypted_in_place = original.clone();

        cipher.encrypt_async_in_place(&mut encrypted_in_place);
        assert_ne!(&encrypted_in_place[..8], &original[..8]);
        assert_eq!(&encrypted_in_place[8..], &original[8..]);

        let key = cipher.tea_key().unwrap();
        tea_decrypt_in_place(&mut encrypted_in_place[..8], &key);
        assert_eq!(encrypted_in_place, original);
        let rendered = format!("{cipher:?}");
        assert!(rendered.contains("client_pubkey"));
        assert!(!rendered.contains("tea_key"));
    }

    impl PrivateKey {
        fn from_str_unchecked_for_test(bytes: [u8; 32]) -> Self {
            Self(bytes)
        }
    }

    trait PrivateParse {
        fn from_str_result(value: &str) -> Result<PrivateKey, CryptoError>;
    }

    impl PrivateParse for PrivateKey {
        fn from_str_result(value: &str) -> Result<PrivateKey, CryptoError> {
            value.parse()
        }
    }

    #[test]
    fn hex_parse_errors_keep_their_source() {
        let err = "zz".repeat(32).parse::<PrivateKey>().unwrap_err();
        assert!(matches!(err, CryptoError::InvalidPrivateKeyHex(_)));
        assert!(err.source().is_some());
    }
}
