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
    #[error("invalid server public key hex: {0}")]
    InvalidServerPubkeyHex(#[from] FromHexError),
    /// The provided key bytes were not valid secp256k1 key material.
    #[error("invalid secp256k1 key material")]
    InvalidKeyMaterial,
}

#[derive(Debug, Clone)]
/// Compatibility cipher that derives a TEA key from an ECDH shared secret.
pub struct EcdhTeaCipher {
    enabled: bool,
    tea_key: [u32; 4],
    client_pubkey: [u8; 64],
}

impl Default for EcdhTeaCipher {
    fn default() -> Self {
        Self {
            enabled: false,
            tea_key: [0; 4],
            client_pubkey: [0; 64],
        }
    }
}

impl EcdhTeaCipher {
    /// Returns a disabled cipher that leaves all inputs unchanged.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Creates a cipher from a remote server public key expressed as raw hex.
    ///
    /// An empty string keeps encryption disabled for parity with legacy callers.
    pub fn new(server_pubkey_hex: &str) -> Result<Self, CryptoError> {
        if server_pubkey_hex.is_empty() {
            return Ok(Self::disabled());
        }
        let secret = SecretKey::random(&mut OsRng);
        Self::from_secret_key(server_pubkey_hex, secret)
    }

    /// Creates a cipher from a caller-supplied private key and server public key.
    pub fn new_with_private_key(
        server_pubkey_hex: &str,
        private_key: [u8; 32],
    ) -> Result<Self, CryptoError> {
        let secret =
            SecretKey::from_slice(&private_key).map_err(|_| CryptoError::InvalidKeyMaterial)?;
        Self::from_secret_key(server_pubkey_hex, secret)
    }

    fn from_secret_key(server_pubkey_hex: &str, secret: SecretKey) -> Result<Self, CryptoError> {
        let server_pubkey = decode_uncompressed_pubkey(server_pubkey_hex)?;

        let shared = diffie_hellman(secret.to_nonzero_scalar(), server_pubkey.as_affine());
        let shared_bytes = shared.raw_secret_bytes();

        let mut tea_key = [0u32; 4];
        for (i, word) in tea_key.iter_mut().enumerate() {
            let start = i * 4;
            *word = u32::from_le_bytes([
                shared_bytes[start],
                shared_bytes[start + 1],
                shared_bytes[start + 2],
                shared_bytes[start + 3],
            ]);
        }

        let point = secret.public_key().to_encoded_point(false);
        let point_bytes = point.as_bytes();
        if point_bytes.len() != 65 {
            return Err(CryptoError::InvalidKeyMaterial);
        }

        let mut client_pubkey = [0u8; 64];
        client_pubkey.copy_from_slice(&point_bytes[1..65]);

        Ok(Self {
            enabled: true,
            tea_key,
            client_pubkey,
        })
    }

    /// Returns whether async encryption is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the client public key sent to the decoder peer.
    pub fn client_pubkey(&self) -> [u8; 64] {
        self.client_pubkey
    }

    /// Returns the derived TEA key as four little-endian words.
    pub fn tea_key_words(&self) -> [u32; 4] {
        self.tea_key
    }

    /// C++ sync path currently writes plaintext; keep parity.
    pub fn encrypt_sync(&self, input: &[u8]) -> Vec<u8> {
        input.to_vec()
    }

    /// Encrypt async chunks block-by-block with TEA and preserve tail bytes.
    pub fn encrypt_async(&self, input: &[u8]) -> Vec<u8> {
        if !self.enabled {
            return input.to_vec();
        }

        let mut out = input.to_vec();
        self.encrypt_async_in_place(&mut out);
        out
    }

    /// Encrypts whole 8-byte blocks in place and leaves any trailing bytes unchanged.
    pub fn encrypt_async_in_place(&self, input: &mut [u8]) {
        if !self.enabled {
            return;
        }
        let block_bytes = input.len() / TEA_BLOCK_LEN * TEA_BLOCK_LEN;
        tea_encrypt_in_place(&mut input[..block_bytes], &self.tea_key);
    }
}

fn decode_uncompressed_pubkey(server_pubkey_hex: &str) -> Result<PublicKey, CryptoError> {
    if server_pubkey_hex.len() != 128 {
        return Err(CryptoError::InvalidServerPubkeyLength);
    }

    let mut raw = [0u8; 64];
    hex::decode_to_slice(server_pubkey_hex, &mut raw)?;

    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(&raw);
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
    use super::{tea_decrypt_in_place, CryptoError, EcdhTeaCipher};

    const SAMPLE_PUBKEY: &str =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8";

    #[test]
    fn disabled_cipher_is_a_noop() {
        let cipher = EcdhTeaCipher::disabled();
        let plain = b"hello world";
        let mut in_place = plain.to_vec();

        assert!(!cipher.enabled());
        assert_eq!(cipher.client_pubkey(), [0; 64]);
        assert_eq!(cipher.encrypt_sync(plain), plain);
        assert_eq!(cipher.encrypt_async(plain), plain);

        cipher.encrypt_async_in_place(&mut in_place);
        assert_eq!(in_place, plain);
        assert!(!EcdhTeaCipher::new("").unwrap().enabled());
    }

    #[test]
    fn constructor_rejects_invalid_key_material() {
        assert!(matches!(
            EcdhTeaCipher::new_with_private_key("abcd", [7u8; 32]),
            Err(CryptoError::InvalidServerPubkeyLength)
        ));
        assert!(matches!(
            EcdhTeaCipher::new_with_private_key(&"z".repeat(128), [7u8; 32]),
            Err(CryptoError::InvalidServerPubkeyHex(_))
        ));
        assert!(matches!(
            EcdhTeaCipher::new_with_private_key(&"00".repeat(64), [7u8; 32]),
            Err(CryptoError::InvalidKeyMaterial)
        ));
        assert!(matches!(
            EcdhTeaCipher::new_with_private_key(SAMPLE_PUBKEY, [0u8; 32]),
            Err(CryptoError::InvalidKeyMaterial)
        ));
    }

    #[test]
    fn async_encryption_preserves_partial_tail_bytes() {
        let cipher = EcdhTeaCipher::new_with_private_key(SAMPLE_PUBKEY, [7u8; 32]).unwrap();
        let original = b"abcdefghXYZ".to_vec();
        let mut encrypted_in_place = original.clone();

        cipher.encrypt_async_in_place(&mut encrypted_in_place);
        assert_ne!(&encrypted_in_place[..8], &original[..8]);
        assert_eq!(&encrypted_in_place[8..], &original[8..]);

        tea_decrypt_in_place(&mut encrypted_in_place[..8], &cipher.tea_key_words());
        assert_eq!(encrypted_in_place, original);

        let mut encrypted = cipher.encrypt_async(&original);
        tea_decrypt_in_place(&mut encrypted[..8], &cipher.tea_key_words());
        assert_eq!(encrypted, original);
    }
}
