#![allow(dead_code)]
//! Hybrid post-quantum-resistant cipher.
//!
//! # Scheme
//! - **Inner layer**: ChaCha20-Poly1305 (ARM-native, no hardware needed, constant-time)
//! - **Outer layer**: AES-256-GCM (AES-NI on x86/ARMv8, hardware-accelerated)
//! - **KDF**: BLAKE3 with domain separation (sponge construction, quantum-safe)
//!
//! 256-bit keys → 128-bit post-quantum security (Grover halves keyspace).
//! Attacker must independently break both AEAD layers.
//!
//! # Wire format (in-session)
//! `[1B version][12B aes_nonce][12B chacha_nonce][AES-GCM(ChaCha20(plaintext))]`
//!
//! # Backup format (self-contained)
//! `[4B magic "CGRP"][1B version][32B random backup_key][cipher_payload…]`
//! Security relies on Telegram DM delivery to admin — key is embedded for portability.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce as AesNonce,
};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaChaKey, Nonce as ChaChaNonce};
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

use crate::error::{BotError, Result};

const VERSION: u8 = 0x01;
const AES_NONCE_LEN: usize = 12;
const CHACHA_NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 1 + AES_NONCE_LEN + CHACHA_NONCE_LEN;

const BACKUP_MAGIC: &[u8; 4] = b"CGRP";
const BACKUP_KEY_LEN: usize = 32;
const BACKUP_HEADER_LEN: usize = 4 + 1 + BACKUP_KEY_LEN;

pub struct HybridCipher {
    aes: Aes256Gcm,
    chacha: ChaCha20Poly1305,
}

impl HybridCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        let aes_key = derive_key(key, "cryptograph:v1:aes-256-gcm");
        let chacha_key = derive_key(key, "cryptograph:v1:chacha20-poly1305");

        Self {
            aes: Aes256Gcm::new_from_slice(&aes_key).expect("32-byte key"),
            chacha: ChaCha20Poly1305::new(ChaChaKey::from_slice(&chacha_key)),
        }
    }

    /// Encrypt plaintext with double-AEAD. Returns wire-format bytes.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut aes_nonce_bytes = [0u8; AES_NONCE_LEN];
        let mut chacha_nonce_bytes = [0u8; CHACHA_NONCE_LEN];
        OsRng.fill_bytes(&mut aes_nonce_bytes);
        OsRng.fill_bytes(&mut chacha_nonce_bytes);

        // Inner: ChaCha20-Poly1305
        let ct_inner = self
            .chacha
            .encrypt(ChaChaNonce::from_slice(&chacha_nonce_bytes), plaintext)
            .map_err(|e| BotError::Encryption(e.to_string()))?;

        // Outer: AES-256-GCM
        let ct_outer = self
            .aes
            .encrypt(AesNonce::from_slice(&aes_nonce_bytes), ct_inner.as_slice())
            .map_err(|e| BotError::Encryption(e.to_string()))?;

        let mut out = Vec::with_capacity(HEADER_LEN + ct_outer.len());
        out.push(VERSION);
        out.extend_from_slice(&aes_nonce_bytes);
        out.extend_from_slice(&chacha_nonce_bytes);
        out.extend_from_slice(&ct_outer);
        Ok(out)
    }

    /// Decrypt wire-format bytes. Returns zeroized plaintext.
    pub fn decrypt(&self, data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if data.len() < HEADER_LEN {
            return Err(BotError::DecryptionFailed);
        }
        if data[0] != VERSION {
            return Err(BotError::DecryptionFailed);
        }

        let aes_nonce = &data[1..1 + AES_NONCE_LEN];
        let chacha_nonce = &data[1 + AES_NONCE_LEN..HEADER_LEN];
        let ciphertext = &data[HEADER_LEN..];

        // Outer: AES-256-GCM
        let ct_inner = self
            .aes
            .decrypt(AesNonce::from_slice(aes_nonce), ciphertext)
            .map_err(|_| BotError::DecryptionFailed)?;

        // Inner: ChaCha20-Poly1305
        let plaintext = self
            .chacha
            .decrypt(ChaChaNonce::from_slice(chacha_nonce), ct_inner.as_slice())
            .map_err(|_| BotError::DecryptionFailed)?;

        Ok(Zeroizing::new(plaintext))
    }
}

// ── Backup helpers (free functions — no session state needed) ────────────────

/// Produce a self-contained backup blob with an embedded random key.
pub fn encrypt_backup(plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut backup_key = Zeroizing::new([0u8; BACKUP_KEY_LEN]);
    OsRng.fill_bytes(backup_key.as_mut());

    let cipher = HybridCipher::new(&backup_key);
    let payload = cipher.encrypt(plaintext)?;

    let mut out = Vec::with_capacity(BACKUP_HEADER_LEN + payload.len());
    out.extend_from_slice(BACKUP_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(backup_key.as_ref());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decrypt a self-contained backup blob. Validates magic + version.
pub fn decrypt_backup(data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if data.len() < BACKUP_HEADER_LEN + HEADER_LEN {
        return Err(BotError::InvalidBackup);
    }
    if &data[..4] != BACKUP_MAGIC {
        return Err(BotError::InvalidBackup);
    }
    if data[4] != VERSION {
        return Err(BotError::InvalidBackup);
    }

    let backup_key: [u8; BACKUP_KEY_LEN] = data[5..5 + BACKUP_KEY_LEN]
        .try_into()
        .map_err(|_| BotError::InvalidBackup)?;

    let cipher = HybridCipher::new(&backup_key);
    cipher.decrypt(&data[BACKUP_HEADER_LEN..])
}

// ── Key derivation ────────────────────────────────────────────────────────────

#[inline]
fn derive_key(session_key: &[u8; 32], context: &str) -> [u8; 32] {
    blake3::derive_key(context, session_key)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: [u8; 32] = [0x4a; 32];

    #[test]
    fn round_trip_encrypt_decrypt() {
        let c = HybridCipher::new(&TEST_KEY);
        let plain = b"hello cryptograph hybrid cipher";
        let enc = c.encrypt(plain).unwrap();
        let dec = c.decrypt(&enc).unwrap();
        assert_eq!(&*dec, plain);
    }

    #[test]
    fn nonces_are_unique_per_call() {
        let c = HybridCipher::new(&TEST_KEY);
        let a = c.encrypt(b"data").unwrap();
        let b = c.encrypt(b"data").unwrap();
        // AES nonces (bytes 1..13) must differ
        assert_ne!(&a[1..13], &b[1..13], "AES nonces must differ");
        // ChaCha nonces (bytes 13..25) must differ
        assert_ne!(&a[13..25], &b[13..25], "ChaCha nonces must differ");
    }

    #[test]
    fn wrong_key_rejected() {
        let c1 = HybridCipher::new(&TEST_KEY);
        let mut bad = TEST_KEY;
        bad[0] ^= 0xff;
        let c2 = HybridCipher::new(&bad);

        let enc = c1.encrypt(b"secret").unwrap();
        assert!(c2.decrypt(&enc).is_err());
    }

    #[test]
    fn truncated_data_rejected() {
        let c = HybridCipher::new(&TEST_KEY);
        let enc = c.encrypt(b"secret").unwrap();
        assert!(c.decrypt(&enc[..5]).is_err());
    }

    #[test]
    fn backup_round_trip() {
        let plain = b"portfolio backup data";
        let blob = encrypt_backup(plain).unwrap();
        let dec = decrypt_backup(&blob).unwrap();
        assert_eq!(&*dec, plain);
    }

    #[test]
    fn backup_bad_magic_rejected() {
        let mut blob = encrypt_backup(b"data").unwrap();
        blob[0] = 0xff;
        assert!(decrypt_backup(&blob).is_err());
    }

    #[test]
    fn backup_each_has_unique_key() {
        let a = encrypt_backup(b"data").unwrap();
        let b = encrypt_backup(b"data").unwrap();
        // backup_key is at bytes 5..37
        assert_ne!(&a[5..37], &b[5..37], "each backup must have a unique key");
    }
}
