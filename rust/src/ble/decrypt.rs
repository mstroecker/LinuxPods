//! AES decryption of the encrypted portion of a proximity pairing advertisement.
//!
//! Port of internal/ble/decrypt.go. Single-block AES-128 (ECB); there is no IV or
//! nonce in the advertisement, so the raw block cipher is used directly.

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};

/// Bytes 9..25 of the advertisement payload.
pub const ENCRYPTED_LEN: usize = 16;
pub const KEY_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum DecryptError {
    #[error("encrypted data must be {ENCRYPTED_LEN} bytes, got {0}")]
    BadDataLen(usize),
    #[error("encryption key must be {KEY_LEN} bytes, got {0}")]
    BadKeyLen(usize),
    /// AES always "succeeds" on a wrong key, so the magic bytes are what actually
    /// tells us whether this key belongs to this device.
    #[error("decryption validation failed: incorrect encryption key")]
    ValidationFailed,
}

/// Decrypts and validates a 16-byte encrypted payload.
///
/// Validation uses the known magic bytes: the upper nibble of byte 0 must be 0x0,
/// and byte 4 must be 0x2D. This is what makes key-guessing across stored devices
/// viable in [`crate::podstate`].
pub fn decrypt_proximity_payload(
    encrypted: &[u8],
    key: &[u8],
) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    if encrypted.len() != ENCRYPTED_LEN {
        return Err(DecryptError::BadDataLen(encrypted.len()));
    }
    if key.len() != KEY_LEN {
        return Err(DecryptError::BadKeyLen(key.len()));
    }

    let cipher =
        Aes128::new_from_slice(key).map_err(|_| DecryptError::BadKeyLen(key.len()))?;
    let mut block = Block::<Aes128>::try_from(encrypted)
        .map_err(|_| DecryptError::BadDataLen(encrypted.len()))?;
    cipher.decrypt_block(&mut block);

    let mut out = [0u8; ENCRYPTED_LEN];
    out.copy_from_slice(block.as_slice());
    if (out[0] & 0xF0) != 0 || out[4] != 0x2D {
        return Err(DecryptError::ValidationFailed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_lengths() {
        assert!(matches!(
            decrypt_proximity_payload(&[0u8; 8], &[0u8; 16]),
            Err(DecryptError::BadDataLen(8))
        ));
        assert!(matches!(
            decrypt_proximity_payload(&[0u8; 16], &[0u8; 8]),
            Err(DecryptError::BadKeyLen(8))
        ));
    }

    #[test]
    fn rejects_garbage_from_wrong_key() {
        // A random block under an arbitrary key will essentially never satisfy both
        // magic-byte constraints, which is the property the identification relies on.
        let err = decrypt_proximity_payload(&[0xAB; 16], &[0xCD; 16]);
        assert!(matches!(err, Err(DecryptError::ValidationFailed)));
    }

    #[test]
    fn accepts_payload_that_decrypts_to_valid_magic() {
        use aes::cipher::BlockCipherEncrypt;
        // Construct a plaintext with valid magic, encrypt it, and check we recover it.
        let key = [0x11u8; 16];
        let mut plain = [0u8; 16];
        plain[0] = 0x03; // upper nibble must be 0
        plain[4] = 0x2D; // magic marker
        plain[1] = 0x55;

        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut block = Block::<Aes128>::try_from(&plain[..]).unwrap();
        cipher.encrypt_block(&mut block);

        let got = decrypt_proximity_payload(block.as_slice(), &key).unwrap();
        assert_eq!(got, plain);
    }
}
