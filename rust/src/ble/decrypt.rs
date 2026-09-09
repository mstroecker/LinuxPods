//! AES decryption of the encrypted portion of a proximity pairing advertisement.
//!
//! Port of internal/ble/decrypt.go. Single-block AES-128 (ECB); there is no IV or
//! nonce in the advertisement, so the raw block cipher is used directly.

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};

/// Bytes 9..25 of the advertisement payload.
pub const ENCRYPTED_LEN: usize = 16;
pub const KEY_LEN: usize = 16;

/// Where the device's MAC suffix sits inside the decrypted payload.
const MAC_SUFFIX_RANGE: std::ops::Range<usize> = 7..10;

/// Byte 4 marker used by pre-Pro-3 models.
const LEGACY_MAGIC: u8 = 0x2D;

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

/// Decrypts a 16-byte payload and checks it against the device the key belongs to.
///
/// Prefer this over [`decrypt_proximity_payload`]: the decrypted payload embeds the
/// last three bytes of the device's real MAC at offset 7, a far stronger signal than
/// the legacy magic bytes and exactly what identification needs.
///
/// Observed on AirPods Pro 3 (model 0x2720), where bytes 0..12 are stable across
/// advertisements and only the trailing four rotate:
///   `10 be be 9f 1d 7d 64 [DD EE FF] 00 00 a7 8a b5 cc`
///                          ^^^^^^^^ MAC suffix of AA:BB:CC:DD:EE:FF
pub fn decrypt_for_device(
    encrypted: &[u8],
    key: &[u8],
    mac_addr: &str,
) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    let decrypted = decrypt_block_only(encrypted, key)?;

    if matches_device(&decrypted, mac_addr) || has_legacy_magic(&decrypted) {
        Ok(decrypted)
    } else {
        Err(DecryptError::ValidationFailed)
    }
}

/// True when the payload carries the last three bytes of `mac_addr` at offset 7.
///
/// Three exact bytes make a false positive about one in 16 million per key tried.
pub fn matches_device(decrypted: &[u8; ENCRYPTED_LEN], mac_addr: &str) -> bool {
    let Some(suffix) = mac_suffix(mac_addr) else {
        return false;
    };
    decrypted[MAC_SUFFIX_RANGE] == suffix
}

/// The pre-Pro-3 marker: upper nibble of byte 0 clear, byte 4 == 0x2D.
///
/// AirPods Pro 3 does not satisfy this (it reports byte 0 = 0x10, byte 4 = 0x1D),
/// so it is kept only as a fallback for older models.
pub fn has_legacy_magic(decrypted: &[u8; ENCRYPTED_LEN]) -> bool {
    (decrypted[0] & 0xF0) == 0 && decrypted[4] == LEGACY_MAGIC
}

/// Parses "AA:BB:CC:DD:EE:FF" into its last three bytes.
fn mac_suffix(mac_addr: &str) -> Option<[u8; 3]> {
    let mut bytes = mac_addr
        .split(':')
        .map(|b| u8::from_str_radix(b, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    if bytes.len() != 6 {
        return None;
    }
    let tail = bytes.split_off(3);
    Some([tail[0], tail[1], tail[2]])
}

/// Decrypts without any validation. Exposed for probes and tests.
pub fn decrypt_raw(encrypted: &[u8], key: &[u8]) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    decrypt_block_only(encrypted, key)
}

/// Decrypts and validates using the legacy magic bytes only.
///
/// Retained for older models and callers without a candidate MAC.
pub fn decrypt_proximity_payload(
    encrypted: &[u8],
    key: &[u8],
) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    let out = decrypt_block_only(encrypted, key)?;
    if !has_legacy_magic(&out) {
        return Err(DecryptError::ValidationFailed);
    }
    Ok(out)
}

fn decrypt_block_only(encrypted: &[u8], key: &[u8]) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    if encrypted.len() != ENCRYPTED_LEN {
        return Err(DecryptError::BadDataLen(encrypted.len()));
    }
    if key.len() != KEY_LEN {
        return Err(DecryptError::BadKeyLen(key.len()));
    }

    let cipher = Aes128::new_from_slice(key).map_err(|_| DecryptError::BadKeyLen(key.len()))?;
    let mut block = Block::<Aes128>::try_from(encrypted)
        .map_err(|_| DecryptError::BadDataLen(encrypted.len()))?;
    cipher.decrypt_block(&mut block);

    let mut out = [0u8; ENCRYPTED_LEN];
    out.copy_from_slice(block.as_slice());
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

    /// Builds ciphertext for a chosen plaintext under a chosen key, so the tests
    /// carry no real device key.
    fn encrypt(plain: &[u8; 16], key: &[u8; 16]) -> [u8; 16] {
        use aes::cipher::BlockCipherEncrypt;
        let cipher = Aes128::new_from_slice(key).unwrap();
        let mut block = Block::<Aes128>::try_from(&plain[..]).unwrap();
        cipher.encrypt_block(&mut block);
        let mut out = [0u8; 16];
        out.copy_from_slice(block.as_slice());
        out
    }

    /// Layout observed on AirPods Pro 3: batteries at 1..4, MAC suffix at 7..10.
    /// Deliberately fails the legacy magic check (byte 0 = 0x10, byte 4 = 0x1D).
    fn pro3_plaintext(mac_suffix: [u8; 3]) -> [u8; 16] {
        let mut p = [0u8; 16];
        p[0] = 0x10;
        p[1] = 0x80 | 62; // left: charging, 62%
        p[2] = 0x80 | 62; // right: charging, 62%
        p[3] = 0x80 | 31; // case: charging, 31%
        p[4] = 0x1D;
        p[5] = 0x7D;
        p[6] = 0x64;
        p[7..10].copy_from_slice(&mac_suffix);
        p[12..16].copy_from_slice(&[0xA7, 0x8A, 0xB5, 0xCC]); // rotating tail
        p
    }

    #[test]
    fn accepts_pro3_layout_via_mac_suffix() {
        let key = [0x11u8; 16];
        let mac = "AA:BB:CC:DD:EE:FF";
        let plain = pro3_plaintext([0xDD, 0xEE, 0xFF]);
        let ct = encrypt(&plain, &key);

        // The legacy validator rejects this model outright...
        assert!(matches!(
            decrypt_proximity_payload(&ct, &key),
            Err(DecryptError::ValidationFailed)
        ));
        // ...while matching against the device's own MAC accepts it.
        assert_eq!(decrypt_for_device(&ct, &key, mac).unwrap(), plain);
    }

    #[test]
    fn rejects_payload_belonging_to_another_device() {
        let key = [0x11u8; 16];
        let ct = encrypt(&pro3_plaintext([0xDD, 0xEE, 0xFF]), &key);
        assert!(matches!(
            decrypt_for_device(&ct, &key, "99:88:77:66:55:44"),
            Err(DecryptError::ValidationFailed)
        ));
    }

    #[test]
    fn legacy_models_still_validate_without_mac_match() {
        let key = [0x22u8; 16];
        let mut plain = [0u8; 16];
        plain[0] = 0x03; // upper nibble clear
        plain[4] = LEGACY_MAGIC;
        let ct = encrypt(&plain, &key);

        // MAC does not match, but the legacy magic carries it.
        assert_eq!(decrypt_for_device(&ct, &key, "99:88:77:66:55:44").unwrap(), plain);
    }

    #[test]
    fn parses_mac_suffix() {
        assert_eq!(mac_suffix("AA:BB:CC:DD:EE:FF"), Some([0xDD, 0xEE, 0xFF]));
        assert_eq!(mac_suffix("not-a-mac"), None);
        assert_eq!(mac_suffix("34:0E:22"), None);
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
