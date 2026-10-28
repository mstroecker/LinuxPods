//! Resolvable private addresses: whether an advertising address is a pair's.
//!
//! AirPods advertise from resolvable private addresses (RPAs): a new one per
//! pod every 4 to 8 minutes, connected or not, and new ones across a closed
//! lid (#71). An RPA is `prand || hash`, 24 bits each, with
//! `hash = ah(IRK, prand)` (Core spec Vol 3 Part H 2.2.2), so only the holder
//! of the pair's IRK can tell that an address is the pair's. The pair sends
//! its IRK over AAP, beside the ENC_KEY (`aap::keys`), and the coordinator
//! counts an advertisement only from an address an IRK resolves (#134).
//!
//! # Byte order
//!
//! Addresses are most significant byte first, as [`Address`] holds them and
//! as they are written: the top two bits of byte 0 are `01`, `prand` is bytes
//! 0-2 and the hash bytes 3-5.
//!
//! The key packet carries the IRK least significant byte first, as SMP
//! carries keys, and `ah` takes it most significant byte first. So
//! [`resolves`] takes the IRK as AAP sends it - as the keystore keeps it -
//! and reverses it. `examples/irk_probe.rs` settled that on AirPods Pro 3 and
//! Pro 2 (USB-C): every one of their addresses resolved with the IRK reversed,
//! and none with it as sent.
//!
//! [`ResolvedAddresses`] remembers which addresses resolved, so that an
//! address costs one resolve per stored IRK, not one per advertisement.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aes::Aes128;
use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};
use bluer::Address;

use crate::KEY_LEN;

/// How long [`ResolvedAddresses`] remembers an address, from when it
/// resolved. Longer than the 4 to 8 minutes an address lasts, so a live
/// address is resolved about once; one heard again later is resolved again,
/// which costs an AES block per stored IRK.
pub const RESOLVED_TTL: Duration = Duration::from_secs(10 * 60);

/// How many addresses [`ResolvedAddresses`] holds at most. A pair has an
/// address per pod, and two or three more per pod within [`RESOLVED_TTL`] as
/// they rotate, so this covers a dozen pairs in range; past it, the oldest
/// goes.
pub const RESOLVED_CAPACITY: usize = 64;

/// True for a resolvable private address: the top two bits are `01`.
pub fn is_resolvable(addr: Address) -> bool {
    addr[0] >> 6 == 0b01
}

/// Whether `irk` resolves `addr`: `addr` is a resolvable private address
/// whose hash is `ah` of its `prand` under the IRK.
///
/// `irk` is as the AAP key packet carries it, least significant byte first -
/// see the module docs.
pub fn resolves(irk: &[u8; KEY_LEN], addr: Address) -> bool {
    is_resolvable(addr)
        && ah(&ah_key(irk), [addr[0], addr[1], addr[2]]) == [addr[3], addr[4], addr[5]]
}

/// The key `ah` takes, most significant byte first, from an IRK as AAP sends
/// it.
fn ah_key(irk: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let mut key = *irk;
    key.reverse();
    key
}

/// The random address hash `ah(k, r)` (Core spec Vol 3 Part H 2.2.2): the low
/// 24 bits of `e(k, 0^104 || r)`. Key, `prand` and result are most
/// significant byte first.
fn ah(key: &[u8; KEY_LEN], prand: [u8; 3]) -> [u8; 3] {
    let cipher = Aes128::new_from_slice(key).expect("an IRK is 16 bytes");
    let mut plain = [0u8; 16];
    plain[13..].copy_from_slice(&prand);
    let mut block = Block::<Aes128>::try_from(&plain[..]).expect("a block is 16 bytes");
    cipher.encrypt_block(&mut block);
    [block[13], block[14], block[15]]
}

/// The resolvable private address `irk` makes of `prand`, as a pair holding
/// that IRK would advertise from: what [`resolves`] takes back, for a fake
/// pair to advertise from (#134).
///
/// `irk` is as AAP sends it. The top two bits of `prand` are set to `01`. A
/// random part of all zeros or all ones, which the specification rules out,
/// has its lowest bit flipped.
#[cfg(any(test, feature = "harness"))]
pub fn resolvable_address(irk: &[u8; KEY_LEN], prand: [u8; 3]) -> Address {
    let mut prand = [prand[0] & 0x3F | 0x40, prand[1], prand[2]];
    let random = [prand[0] & 0x3F, prand[1], prand[2]];
    if random == [0, 0, 0] || random == [0x3F, 0xFF, 0xFF] {
        prand[2] ^= 0x01;
    }
    let hash = ah(&ah_key(irk), prand);
    Address::new([prand[0], prand[1], prand[2], hash[0], hash[1], hash[2]])
}

/// Advertising addresses an IRK resolved, and the pair it is: what spares
/// an address a resolve per advertisement.
///
/// Only addresses that resolved go in, so nothing an advertiser sends can
/// grow it: an address no IRK resolves is not remembered, and is resolved
/// again with each advertisement. It is bounded by [`RESOLVED_CAPACITY`], the
/// oldest going first, and each entry expires [`RESOLVED_TTL`] after it
/// resolved.
///
/// Pure, as `aap::noise::ModeState` is: the time comes in as an argument.
#[derive(Default)]
pub struct ResolvedAddresses {
    entries: HashMap<Address, Resolved>,
}

struct Resolved {
    /// The pair's real address.
    pair: Address,
    /// The IRK that resolved it. A pair whose IRK has changed since -
    /// re-paired - is not this address's any more.
    irk: [u8; KEY_LEN],
    at: Instant,
}

impl ResolvedAddresses {
    /// The pair `addr` resolved to, and with which IRK, unless that was
    /// [`RESOLVED_TTL`] or more before `now`.
    pub fn get(&self, addr: Address, now: Instant) -> Option<(Address, [u8; KEY_LEN])> {
        self.entries
            .get(&addr)
            .filter(|e| !expired(e, now))
            .map(|e| (e.pair, e.irk))
    }

    /// Records that `irk`, `pair`'s, resolved `addr` at `now`. The expired
    /// entries go first, then the oldest if it is still full.
    pub fn insert(&mut self, addr: Address, pair: Address, irk: [u8; KEY_LEN], now: Instant) {
        self.entries.retain(|_, e| !expired(e, now));
        if self.entries.len() >= RESOLVED_CAPACITY
            && !self.entries.contains_key(&addr)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(&a, _)| a)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(addr, Resolved { pair, irk, at: now });
    }

    /// How many addresses it holds, expired ones included until the next
    /// insert.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn expired(entry: &Resolved, now: Instant) -> bool {
    now.saturating_duration_since(entry.at) >= RESOLVED_TTL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::decrypt::random_key;

    /// The IRK of the sample data in Core spec Vol 3 Part H Appendix D.7,
    /// reversed into the order AAP sends an IRK in. Public, and nobody's key.
    const SPEC_IRK_AS_SENT: [u8; KEY_LEN] = [
        0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02,
        0xec,
    ]; // gitleaks:allow - public sample data, not a device key

    /// The sample's `prand` 708194 and hash 0dfbaa, as an address.
    const SPEC_ADDRESS: Address = Address::new([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xaa]);

    #[test]
    fn the_specification_sample_resolves_with_the_irk_as_aap_sends_it() {
        assert!(resolves(&SPEC_IRK_AS_SENT, SPEC_ADDRESS));
    }

    /// The order is pinned: the IRK in the order the specification writes
    /// it, which is not how AAP sends it, resolves nothing.
    #[test]
    fn the_irk_in_the_specifications_order_does_not_resolve() {
        let mut spec_order = SPEC_IRK_AS_SENT;
        spec_order.reverse();
        assert!(!resolves(&spec_order, SPEC_ADDRESS));
    }

    #[test]
    fn a_wrong_hash_does_not_resolve() {
        for byte in 3..6 {
            let mut addr = SPEC_ADDRESS.0;
            addr[byte] ^= 0x01;
            assert!(
                !resolves(&SPEC_IRK_AS_SENT, Address::new(addr)),
                "byte {byte}"
            );
        }
    }

    /// Only an address whose top bits are `01` is resolvable: the sample's
    /// bytes as any other address type resolve nothing, though the hash
    /// would match.
    #[test]
    fn another_address_type_does_not_resolve() {
        for top in [0x00, 0x80, 0xC0] {
            let mut addr = SPEC_ADDRESS.0;
            addr[0] = addr[0] & 0x3F | top;
            let addr = Address::new(addr);
            assert!(!is_resolvable(addr));
            assert!(!resolves(&SPEC_IRK_AS_SENT, addr), "top bits {top:#04x}");
        }
    }

    #[test]
    fn a_made_address_resolves_with_its_irk_only() {
        let irk = random_key();
        let other = random_key();
        let addr = resolvable_address(&irk, [0xFF, 0x12, 0x34]);
        assert!(is_resolvable(addr));
        assert_eq!(addr[0] & 0x3F, 0x3F, "the random part is kept");
        assert!(resolves(&irk, addr));
        assert!(!resolves(&other, addr));
        assert_eq!(
            resolvable_address(&SPEC_IRK_AS_SENT, [0x70, 0x81, 0x94]),
            SPEC_ADDRESS
        );
    }

    #[test]
    fn a_made_address_never_has_an_all_zero_or_all_one_random_part() {
        let irk = random_key();
        for prand in [
            [0x00, 0x00, 0x00],
            [0xFF, 0xFF, 0xFF],
            [0x40, 0, 0],
            [0x7F, 0xFF, 0xFF],
        ] {
            let addr = resolvable_address(&irk, prand);
            let random = [addr[0] & 0x3F, addr[1], addr[2]];
            assert!(
                random != [0, 0, 0] && random != [0x3F, 0xFF, 0xFF],
                "{prand:02x?}"
            );
            assert!(resolves(&irk, addr));
        }
    }

    fn nth(i: usize) -> Address {
        let [.., hi, lo] = i.to_be_bytes();
        Address::new([0x40, 0, 0, 0, hi, lo])
    }

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);

    #[test]
    fn an_address_is_remembered_until_it_expires() {
        let irk = random_key();
        let start = Instant::now();
        let mut cache = ResolvedAddresses::default();
        cache.insert(SPEC_ADDRESS, PAIR, irk, start);

        assert_eq!(cache.get(SPEC_ADDRESS, start), Some((PAIR, irk)));
        let almost = start + RESOLVED_TTL - Duration::from_millis(1);
        assert_eq!(cache.get(SPEC_ADDRESS, almost), Some((PAIR, irk)));
        assert_eq!(cache.get(SPEC_ADDRESS, start + RESOLVED_TTL), None);
        assert_eq!(cache.get(nth(1), start), None, "never resolved");
    }

    /// However many addresses resolve, it holds no more than its capacity,
    /// and the oldest go first.
    #[test]
    fn it_is_bounded_and_the_oldest_goes_first() {
        let irk = random_key();
        let start = Instant::now();
        let mut cache = ResolvedAddresses::default();
        for i in 0..RESOLVED_CAPACITY * 4 {
            cache.insert(nth(i), PAIR, irk, start + Duration::from_millis(i as u64));
        }
        assert_eq!(cache.len(), RESOLVED_CAPACITY);
        let now = start + Duration::from_secs(1);
        let newest = RESOLVED_CAPACITY * 4 - 1;
        assert!(cache.get(nth(newest), now).is_some());
        assert!(
            cache
                .get(nth(newest + 1 - RESOLVED_CAPACITY), now)
                .is_some()
        );
        assert!(cache.get(nth(newest - RESOLVED_CAPACITY), now).is_none());
    }

    /// An insert clears out what expired, rather than keeping it until the
    /// cache is full.
    #[test]
    fn an_insert_drops_the_expired() {
        let irk = random_key();
        let start = Instant::now();
        let mut cache = ResolvedAddresses::default();
        cache.insert(nth(1), PAIR, irk, start);
        cache.insert(nth(2), PAIR, irk, start + RESOLVED_TTL);
        assert_eq!(cache.len(), 1);
    }

    /// Resolved again, an address is remembered from then, and takes no
    /// room of another's.
    #[test]
    fn an_address_resolved_again_is_replaced() {
        let (old, new) = (random_key(), random_key());
        let start = Instant::now();
        let mut cache = ResolvedAddresses::default();
        for i in 0..RESOLVED_CAPACITY {
            cache.insert(nth(i), PAIR, old, start);
        }
        let later = start + Duration::from_secs(1);
        cache.insert(nth(0), PAIR, new, later);
        assert_eq!(cache.len(), RESOLVED_CAPACITY);
        assert_eq!(cache.get(nth(0), later), Some((PAIR, new)));
        assert!(cache.get(nth(1), later).is_some(), "nothing else went");
    }
}
