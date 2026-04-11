use anyhow::Result;
use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;

const SALT: &[u8] = b"screenshare-p2p-v1-salt!";

pub struct Crypto {
    cipher: ChaCha20Poly1305,
}

impl Crypto {
    pub fn from_passphrase(passphrase: &str) -> Result<Self> {
        let mut key_bytes = [0u8; 32];
        Argon2::default()
            .hash_password_into(passphrase.as_bytes(), SALT, &mut key_bytes)
            .map_err(|e| anyhow::anyhow!("Key derivation failed: {e}"))?;

        let key = Key::from_slice(&key_bytes);
        let cipher = ChaCha20Poly1305::new(key);

        Ok(Self { cipher })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

        // Prepend nonce to ciphertext
        let mut result = Vec::with_capacity(12 + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        Ok(result)
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(data.len() > 12, "Ciphertext too short");

        let nonce = Nonce::from_slice(&data[..12]);
        let ciphertext = &data[12..];

        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("Decryption failed (wrong passphrase?): {e}"))
    }
}
