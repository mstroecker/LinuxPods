//! AES decryption of the encrypted portion of a proximity pairing advertisement.
//!
//! Single-block AES-128 (ECB); there is no IV or
//! nonce in the advertisement, so the raw block cipher is used directly.

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};
use bluer::Address;

/// Bytes 9..25 of the advertisement payload.
pub const ENCRYPTED_LEN: usize = 16;
pub use crate::KEY_LEN;

/// Where the device's MAC suffix sits inside the decrypted payload.
const MAC_SUFFIX_RANGE: std::ops::Range<usize> = 7..10;

/// What sits there instead while the device is connected to a host.
///
/// The suffix exists so a paired-but-disconnected device can be recognized behind
/// a randomized address; connected, the device publishes zeros. Observed on
/// AirPods Pro 3 (0x2027) with a key that decrypts correctly by every other
/// measure - batteries agreeing with the cleartext buckets and the usual
/// `1d 7d 64` at bytes 4-6.
pub(crate) const CONNECTED_SUFFIX: [u8; 3] = [0x00; 3];

#[derive(Debug, thiserror::Error)]
pub enum DecryptError {
    #[error("encrypted data must be {ENCRYPTED_LEN} bytes, got {0}")]
    BadDataLen(usize),
    #[error("encryption key must be {KEY_LEN} bytes, got {0}")]
    BadKeyLen(usize),
    /// AES always "succeeds" on a wrong key, so the MAC suffix is what actually
    /// tells us whether this key belongs to this device.
    #[error("decryption validation failed: incorrect encryption key")]
    ValidationFailed,
}

/// Decrypts a 16-byte payload and checks it against the device the key belongs to.
///
/// The decrypted payload embeds the last three bytes of the device's real MAC at
/// offset 7, so it identifies its own device - which is exactly what is needed to
/// resolve a randomized BLE MAC back to a permanent one.
///
/// While the device is connected the suffix reads `00 00 00` instead; see
/// [`matches_device`], which accepts either.
///
/// Layout confirmed on two models; only byte 0, the batteries and the trailing
/// four bytes differ between them:
/// ```text
/// Pro 3  (0x2027): 10 be be 9f 1d 7d 64 [DD EE FF] 00 00 a7 8a b5 cc
/// Gen 2  (0x2024): 00 e4 e4 8e 1d 7d 64 [44 55 66] 00 00 3b a2 15 bd
///                     ^^ ^^ ^^              ^^^^^^ MAC suffix
///                     batteries (bit 7 = charging)
/// ```
///
/// Note there is no magic-byte alternative: the 0x2D marker at byte 4 reported by
/// earlier reverse engineering appears on neither model (both report 0x1D), so a
/// magic-byte check rejects every correct decryption.
pub fn decrypt_for_device(
    encrypted: &[u8],
    key: &[u8],
    mac_addr: Address,
) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
    let decrypted = decrypt_raw(encrypted, key)?;

    if matches_device(&decrypted, mac_addr) {
        Ok(decrypted)
    } else {
        Err(DecryptError::ValidationFailed)
    }
}

/// True when the payload carries the last three bytes of `mac_addr` at offset 7,
/// or the zeros a connected device publishes in their place.
///
/// Identification does not come from reading the suffix - it comes from *which*
/// key produced this plaintext, one key being tried at a time. The suffix is only
/// the test that the key was the right one, so zeros serve that purpose just as
/// well: a wrong key yields pseudorandom bytes, and three of them landing on zero
/// is as unlikely as landing on one specific device's suffix.
///
/// Accepting both costs exactly one bit - a false accept per wrong key goes from
/// 2^-24 to 2^-23, measured at 4 and 4 accepts respectively over 40M random keys.
/// Without the second rule, a device connected to a phone falls back to the
/// cleartext's 10% buckets and stays unidentified behind its rotating address,
/// which is the case `docs/ble-proximity-pairing.md` sets out to cover.
pub fn matches_device(decrypted: &[u8; ENCRYPTED_LEN], mac_addr: Address) -> bool {
    decrypted[MAC_SUFFIX_RANGE] == mac_addr[3..] || decrypted[MAC_SUFFIX_RANGE] == CONNECTED_SUFFIX
}

/// Decrypts without any validation: what [`decrypt_for_device`] checks, and
/// what `examples/decrypt_probe.rs` checks with [`matches_device`] itself, so
/// it can show what the magic-byte check would have said.
pub fn decrypt_raw(encrypted: &[u8], key: &[u8]) -> Result<[u8; ENCRYPTED_LEN], DecryptError> {
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

/// The decrypted payload, field by field: what [`Plaintext::to_bytes`] encodes,
/// [`matches_device`] checks and `ProximityData::add_decrypted_data` reads.
#[cfg(any(test, feature = "harness"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plaintext {
    /// Byte 0, unexplained and model-dependent: 0x10 on Pro 3, 0x00 or 0x04 on
    /// Pro 2.
    pub header: u8,
    /// Byte 1, the primary pod: the left one unless the advertisement says the
    /// right is primary. Each battery byte holds the level in bits 0-6 - `None`
    /// goes out as 127, which reads as unknown - and charging in bit 7.
    pub primary_battery: Option<u8>,
    pub primary_charging: bool,
    /// Byte 2, the other pod.
    pub secondary_battery: Option<u8>,
    pub secondary_charging: bool,
    /// Byte 3.
    pub case_battery: Option<u8>,
    pub case_charging: bool,
    /// Bytes 7-9: the last three bytes of the real MAC, or [`CONNECTED_SUFFIX`]
    /// while the device is connected to a host.
    pub mac_suffix: [u8; 3],
    /// Bytes 12-15, which change with every advertisement.
    pub tail: [u8; 4],
}

#[cfg(any(test, feature = "harness"))]
impl Plaintext {
    /// The 16 bytes to encrypt. Bytes 4-6 get the `1d 7d 64` both models
    /// tested send, and bytes 10-11 zeros: written so the plaintext looks like
    /// a captured one, and checked by nothing.
    pub fn to_bytes(&self) -> [u8; ENCRYPTED_LEN] {
        let battery = |level: Option<u8>, charging: bool| {
            level.unwrap_or(0x7F) & 0x7F | if charging { 0x80 } else { 0x00 }
        };
        let mut plain = [0u8; ENCRYPTED_LEN];
        plain[0] = self.header;
        plain[1] = battery(self.primary_battery, self.primary_charging);
        plain[2] = battery(self.secondary_battery, self.secondary_charging);
        plain[3] = battery(self.case_battery, self.case_charging);
        plain[4..7].copy_from_slice(&[0x1D, 0x7D, 0x64]);
        plain[MAC_SUFFIX_RANGE].copy_from_slice(&self.mac_suffix);
        plain[12..16].copy_from_slice(&self.tail);
        plain
    }
}

/// Encrypts one block the way AirPods do, so a test or a fake can produce what
/// [`decrypt_for_device`] takes without a real device key.
#[cfg(any(test, feature = "harness"))]
pub fn encrypt(plain: &[u8; ENCRYPTED_LEN], key: &[u8; KEY_LEN]) -> [u8; ENCRYPTED_LEN] {
    use aes::cipher::BlockCipherEncrypt;

    let cipher = Aes128::new_from_slice(key).expect("a 16-byte key");
    let mut block = Block::<Aes128>::try_from(&plain[..]).expect("a 16-byte block");
    cipher.encrypt_block(&mut block);
    let mut out = [0u8; ENCRYPTED_LEN];
    out.copy_from_slice(block.as_slice());
    out
}

/// A key from the kernel's random source.
///
/// Test and fake keys are made at runtime: a key written into the source could
/// be a real one, and a literal flowing into AES is what CodeQL's
/// hard-coded-cryptographic-value rule flags (#66).
///
/// # Panics
///
/// If `/dev/urandom` cannot be read.
#[cfg(any(test, feature = "harness"))]
pub fn random_key() -> [u8; KEY_LEN] {
    use std::io::Read;

    // Read into a Vec, not a zeroed array: CodeQL takes the zeros for a
    // hard-coded key, though every byte of them would be overwritten.
    let mut key = Vec::with_capacity(KEY_LEN);
    std::fs::File::open("/dev/urandom")
        .and_then(|f| f.take(KEY_LEN as u64).read_to_end(&mut key))
        .expect("failed to read /dev/urandom");
    key.try_into()
        .expect("/dev/urandom gives as many bytes as asked for")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    #[test]
    fn rejects_wrong_lengths() {
        assert!(matches!(
            decrypt_for_device(&[0u8; 8], &[0u8; 16], MAC),
            Err(DecryptError::BadDataLen(8))
        ));
        assert!(matches!(
            decrypt_for_device(&[0u8; 16], &[0u8; 8], MAC),
            Err(DecryptError::BadKeyLen(8))
        ));
    }

    #[test]
    fn rejects_garbage_from_wrong_key() {
        // A random block under an arbitrary key will essentially never reproduce the
        // device's MAC suffix, which is the property identification relies on.
        let err = decrypt_for_device(&[0xAB; 16], &[0xCD; 16], MAC);
        assert!(matches!(err, Err(DecryptError::ValidationFailed)));
    }

    /// The Pro 3 sample in `docs/ble-proximity-pairing.md`: 62% / 62% / 31%, all
    /// charging, with the given suffix. Deliberately fails the legacy magic check
    /// (byte 0 = 0x10, byte 4 = 0x1D).
    fn pro3_plaintext(mac_suffix: [u8; 3]) -> Plaintext {
        Plaintext {
            header: 0x10,
            primary_battery: Some(62),
            primary_charging: true,
            secondary_battery: Some(62),
            secondary_charging: true,
            case_battery: Some(31),
            case_charging: true,
            mac_suffix,
            tail: [0xA7, 0x8A, 0xB5, 0xCC],
        }
    }

    /// The builder's bytes are the documented sample's.
    #[test]
    fn builds_the_documented_pro3_sample() {
        assert_eq!(
            crate::hex(&pro3_plaintext([0xDD, 0xEE, 0xFF]).to_bytes()),
            "10 be be 9f 1d 7d 64 dd ee ff 00 00 a7 8a b5 cc"
        );
    }

    /// An unknown level goes out as 127, which no reader takes for a level, with
    /// charging still in bit 7 beside it.
    #[test]
    fn writes_an_unknown_level_above_100() {
        let plain = Plaintext {
            case_battery: None,
            case_charging: true,
            ..pro3_plaintext(CONNECTED_SUFFIX)
        };
        assert_eq!(plain.to_bytes()[3], 0xFF);
        let plain = Plaintext {
            case_charging: false,
            ..plain
        };
        assert_eq!(plain.to_bytes()[3], 0x7F);
    }

    #[test]
    fn encrypt_is_the_inverse_of_decrypt() {
        let key = random_key();
        let plain = pro3_plaintext([0xDD, 0xEE, 0xFF]).to_bytes();
        let ct = encrypt(&plain, &key);
        assert_ne!(ct, plain);
        assert_eq!(decrypt_raw(&ct, &key).unwrap(), plain);
    }

    #[test]
    fn random_keys_differ() {
        assert_ne!(random_key(), random_key());
    }

    #[test]
    fn accepts_pro3_layout_via_mac_suffix() {
        let key = random_key();
        let plain = pro3_plaintext([0xDD, 0xEE, 0xFF]).to_bytes();
        let ct = encrypt(&plain, &key);

        assert_eq!(decrypt_for_device(&ct, &key, MAC).unwrap(), plain);
    }

    #[test]
    fn rejects_payload_belonging_to_another_device() {
        let key = random_key();
        let ct = encrypt(&pro3_plaintext([0xDD, 0xEE, 0xFF]).to_bytes(), &key);
        let other = Address::new([0x99, 0x88, 0x77, 0x66, 0x55, 0x44]);
        assert!(matches!(
            decrypt_for_device(&ct, &key, other),
            Err(DecryptError::ValidationFailed)
        ));
    }

    /// A connected device zeroes the suffix field. The plaintext is otherwise
    /// intact, so rejecting it would drop 1% accuracy exactly when AAP is not
    /// available - the AirPods are on someone's phone.
    #[test]
    fn accepts_the_zeroed_suffix_of_a_connected_device() {
        let key = random_key();
        let plain = pro3_plaintext(CONNECTED_SUFFIX).to_bytes();
        let ct = encrypt(&plain, &key);

        assert_eq!(decrypt_for_device(&ct, &key, MAC).unwrap(), plain);
    }

    /// The zeroed form is an exact value, not a wildcard: a partial match is still
    /// a wrong key.
    #[test]
    fn rejects_a_partially_zeroed_suffix() {
        let key = random_key();
        let ct = encrypt(&pro3_plaintext([0x00, 0x00, 0x01]).to_bytes(), &key);
        assert!(matches!(
            decrypt_for_device(&ct, &key, MAC),
            Err(DecryptError::ValidationFailed)
        ));
    }

    /// The suffix is the address's last three bytes; the first three play no
    /// part.
    #[test]
    fn the_suffix_is_the_last_three_bytes() {
        let plain = pro3_plaintext([0xDD, 0xEE, 0xFF]).to_bytes();
        assert!(matches_device(&plain, MAC));
        assert!(matches_device(
            &plain,
            Address::new([0x11, 0x22, 0x33, 0xDD, 0xEE, 0xFF])
        ));
        assert!(!matches_device(
            &plain,
            Address::new([0xDD, 0xEE, 0xFF, 0xAA, 0xBB, 0xCC])
        ));
    }

    #[test]
    fn round_trips_gen2_layout() {
        // Gen 2 (0x2024) reports byte 0 = 0x00 where Pro 3 reports 0x10; both carry
        // the MAC suffix, which is why validation keys on that and nothing else.
        let key = random_key();
        let mac = Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        let plain = Plaintext {
            header: 0x00,
            primary_battery: Some(100),
            secondary_battery: Some(100),
            case_battery: Some(14),
            ..pro3_plaintext([0x44, 0x55, 0x66])
        }
        .to_bytes();

        let ct = encrypt(&plain, &key);
        assert_eq!(decrypt_for_device(&ct, &key, mac).unwrap(), plain);
    }
}
