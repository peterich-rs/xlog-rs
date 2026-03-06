use hex::FromHexError;
use k256::ecdh::diffie_hellman;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::{PublicKey, SecretKey};
use rand_core::OsRng;
use thiserror::Error;

const TEA_BLOCK_LEN: usize = 8;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("server public key must be 128 hex chars")]
    InvalidServerPubkeyLength,
    #[error("invalid server public key hex: {0}")]
    InvalidServerPubkeyHex(#[from] FromHexError),
    #[error("invalid secp256k1 key material")]
    InvalidKeyMaterial,
}

#[derive(Debug, Clone)]
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
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn new(server_pubkey_hex: &str) -> Result<Self, CryptoError> {
        if server_pubkey_hex.is_empty() {
            return Ok(Self::disabled());
        }
        let secret = SecretKey::random(&mut OsRng);
        Self::from_secret_key(server_pubkey_hex, secret)
    }

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
        for i in 0..4 {
            let start = i * 4;
            tea_key[i] = u32::from_le_bytes([
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

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn client_pubkey(&self) -> [u8; 64] {
        self.client_pubkey
    }

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
