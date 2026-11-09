/// Integration tests for the cipher module.
/// These are in a separate test file to keep src/ clean.

#[path = "../src/error.rs"]
pub mod error;

#[path = "../src/security/cipher.rs"]
mod cipher;

use cipher::{decrypt_backup, encrypt_backup, HybridCipher};

const KEY: [u8; 32] = [0xAB; 32];

#[test]
fn round_trip_small() {
    let c = HybridCipher::new(&KEY);
    let plain = b"hello world";
    let enc = c.encrypt(plain).unwrap();
    let dec = c.decrypt(&enc).unwrap();
    assert_eq!(&*dec, plain);
}

#[test]
fn round_trip_large() {
    let c = HybridCipher::new(&KEY);
    let plain = vec![0x42u8; 64 * 1024];
    let enc = c.encrypt(&plain).unwrap();
    let dec = c.decrypt(&enc).unwrap();
    assert_eq!(&*dec, plain.as_slice());
}

#[test]
fn empty_plaintext() {
    let c = HybridCipher::new(&KEY);
    let enc = c.encrypt(b"").unwrap();
    let dec = c.decrypt(&enc).unwrap();
    assert_eq!(&*dec, b"");
}

#[test]
fn wrong_key_rejected() {
    let c1 = HybridCipher::new(&KEY);
    let mut bad = KEY;
    bad[0] ^= 0xFF;
    let c2 = HybridCipher::new(&bad);
    let enc = c1.encrypt(b"secret").unwrap();
    assert!(c2.decrypt(&enc).is_err());
}

#[test]
fn bit_flip_rejected() {
    let c = HybridCipher::new(&KEY);
    let mut enc = c.encrypt(b"secret data").unwrap();
    let len = enc.len();
    enc[len - 1] ^= 1; // flip last byte of ciphertext
    assert!(
        c.decrypt(&enc).is_err(),
        "tampered ciphertext should be rejected"
    );
}

#[test]
fn unique_ciphertexts_per_call() {
    let c = HybridCipher::new(&KEY);
    let a = c.encrypt(b"same input").unwrap();
    let b = c.encrypt(b"same input").unwrap();
    assert_ne!(a, b, "nonces must differ across calls");
}

#[test]
fn backup_round_trip_standalone() {
    let data = b"portfolio json payload";
    let blob = encrypt_backup(data).unwrap();
    let dec = decrypt_backup(&blob).unwrap();
    assert_eq!(&*dec, data);
}

#[test]
fn backup_each_has_unique_embedded_key() {
    let a = encrypt_backup(b"data").unwrap();
    let b = encrypt_backup(b"data").unwrap();
    assert_ne!(
        &a[5..37],
        &b[5..37],
        "each backup must embed a fresh random key"
    );
}

#[test]
fn backup_truncated_rejected() {
    let blob = encrypt_backup(b"data").unwrap();
    assert!(decrypt_backup(&blob[..10]).is_err());
}
