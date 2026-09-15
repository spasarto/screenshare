/// Encryption for signaling blobs, keyed by the session code.
///
/// The signaling server (worker + Durable Object) only ever stores and
/// forwards ciphertext: the offer/answer SDP is encrypted here before
/// upload and decrypted after download, using a key derived from the
/// session code that both peers already share out of band. This buys two
/// things the signaling server itself can't provide:
///
/// - Confidentiality: neither the worker, its logs, nor anything on the
///   path to it ever sees plaintext SDP (which includes ICE candidates,
///   i.e. IP addresses).
/// - Authenticity: AES-GCM is an AEAD, so a blob that wasn't produced by
///   someone holding the session code fails to decrypt. The offerer and
///   answerer treat a decrypt failure as "not ready yet" rather than
///   accepting it, so a forged or accidentally-clobbered answer/offer
///   can never be applied.
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::Rng;
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;

pub struct SessionKey(Key<Aes256Gcm>);

impl SessionKey {
    /// Derive a symmetric key from the session code. The code is the only
    /// secret two peers share, so the key's strength is exactly the code's
    /// entropy — this is not meant to resist an offline guessing attack,
    /// only to keep the signaling server from being able to read or forge
    /// what passes through it.
    pub fn derive(session_code: &str) -> Self {
        let digest = Sha256::digest(session_code.as_bytes());
        Self(Key::<Aes256Gcm>::try_from(digest.as_slice()).expect("SHA-256 digest is 32 bytes"))
    }

    /// Encrypt `plaintext`, returning a base64 blob safe to send as an HTTP body.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let cipher = Aes256Gcm::new(&self.0);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::try_from(nonce_bytes.as_slice()).expect("nonce is NONCE_LEN bytes");

        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("Encryption failed"))?;

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(STANDARD.encode(out))
    }

    /// Decrypt a blob produced by [`SessionKey::encrypt`]. Fails if the blob
    /// is malformed, or was encrypted with a different key (wrong code) or
    /// tampered with in transit.
    pub fn decrypt(&self, blob: &str) -> Result<String> {
        let raw = STANDARD.decode(blob).context("Malformed ciphertext")?;
        if raw.len() < NONCE_LEN {
            anyhow::bail!("Ciphertext too short");
        }
        let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);

        let cipher = Aes256Gcm::new(&self.0);
        let nonce = Nonce::try_from(nonce_bytes).context("Invalid nonce length")?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| anyhow::anyhow!("Decryption failed (wrong code or tampered data)"))?;

        String::from_utf8(plaintext).context("Decrypted payload was not valid UTF-8")
    }
}
