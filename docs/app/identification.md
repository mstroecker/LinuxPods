# Identification

Which advertisements Cymba counts, how it ties each to a pair, and what it drops. The
addresses and the advertisement are the pods':
[protocol/ble/addresses.md](../protocol/ble/addresses.md),
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md). The keys come
over AAP: [keystore.md](keystore.md).

## What counts

**An advertisement counts only if its address resolves with a stored IRK, and that pair's
ENC_KEY decrypts it** (#134). Everything else is dropped on arrival.

1. **Resolve the address** with each stored IRK (`ble::rpa::resolves`, `ah()` of Core spec
   Vol 3 Part H 2.2.2): [protocol/ble/addresses.md](../protocol/ble/addresses.md#resolving-the-advertising-address).
2. **Decrypt with that pair's ENC_KEY only,** never another pair's.
3. **Check the decrypted MAC suffix** ([below](#the-suffix)).
4. **Key the reading by the pair's real MAC** ([below](#keyed-by-the-real-mac)).

So what Cymba shows over BLE is always the exact, decrypted reading. The scanner's
`BLE parsable:` log still records an advertisement that is then dropped
([logging.md](logging.md#ble-advertisements)).

## Why both checks

With the ENC_KEY alone, every stored key was tried on every advertisement, and a wrong key
passes the suffix check one time in 2^23
([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decryption-validation)).
That chance was paid per advertisement and per key:

| Around the user | A false match |
|---|---|
| About ten foreign pairs in range, two stored keys | About every 12 hours |
| A spammer sending random payloads, 1000 a second | Every couple of hours, per key |

A false match gives one of the user's pairs random levels, kept until the pair is next
heard, up to `BLE_CACHE_TTL`, and across a relaunch ([state.md](state.md#the-ble-cache)).

A wrong IRK's chance of resolving an address is 2^-24, paid once per address, not per
advertisement ([protocol/ble/addresses.md](../protocol/ble/addresses.md#false-matches)).
The resolving pair's key, the only one tried, still has to pass the suffix check. A
recorded advertisement replayed from another address counts for nothing.

## Resolved addresses

A resolve costs one AES block per stored IRK. So the coordinator remembers the addresses
that resolved, each with the IRK that resolved it (`rpa::ResolvedAddresses`):

- **Bounded and expiring:** for `ble::rpa::RESOLVED_TTL`, longer than an address lasts,
  and at most `ble::rpa::RESOLVED_CAPACITY` of them, the oldest going first. An address is
  resolved about once.
- **Checked again on every hit** against the IRK stored now.
- **Only addresses that resolved.** An address no IRK resolves is never remembered, so a
  spammer rotating addresses grows nothing. It is resolved again with each advertisement,
  at one AES block per stored IRK.
- **A std mutex beside `inner`,** taken alone and never held across an await.

## Pairs with no IRK

A keystore file of format version 1 holds ENC_KEYs and no IRKs
([keystore.md](keystore.md#the-file)). A pair keyed in one has no IRK until its next AAP
connection brings one, as the keys are requested on every connection. Until then it is
identified by its ENC_KEY alone: its key is tried on every advertisement no stored IRK
resolves. The same holds for a pair whose key packet brought a new ENC_KEY and
no usable IRK ([keystore.md](keystore.md#which-keys-are-taken)).

## The suffix

What the suffix holds, and when the pods zero it:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decryption-validation).

**The suffix is the only check of the plaintext.** It must be the pair's MAC suffix or
`00 00 00`.

- **`00 00 00` is accepted.** The pods zero the field unless both are in the case,
  connected or not (#71). Rejecting it left the pair unidentified whenever a pod was out of
  the case, and so dropped: the pair vanished from BLE in exactly the case Cymba sets
  out to support.
- **Identification never came from reading the suffix.** It comes from the IRK that
  resolved the address and the key that decrypted the payload, one key tried at a time.
  Accepting both values costs one bit: 2^-23 per wrong key.

⚠️ **Do not validate with magic bytes.** The older check - byte 0's upper nibble `0x0`,
byte 4 `0x2D` - holds on neither a Pro 3 nor a Pro 2 (USB-C), and rejected every correct
decryption ([the warning](../protocol/ble/proximity-pairing.md#decryption-validation)).
As an unidentified advertisement is dropped, nothing showed over BLE at all.

## Where the ciphertext is

Cymba takes the fixed range `payload[9..25]` (`ble::parser::ENCRYPTED_RANGE`), right
after the last cleartext byte. Where it sits in the payloads seen:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#bytes-9-24-encrypted-battery-data).

## Batteries in the clear and encrypted

The two kinds of level:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#batteries-in-the-clear-and-encrypted).

The parser reads the cleartext levels, then replaces them with the decrypted ones. An
advertisement no stored key identifies is dropped, so the cleartext levels are never shown
today: what the keys give is not precision but identification. The cleartext is still
decoded, as it is all an unidentified pair advertises: offering such a pair for pairing
would read it (#131).

## Keyed by the real MAC

How a pair's addresses map to it, step by step:
[protocol/ble/addresses.md](../protocol/ble/addresses.md#multi-device-workflow).

Identification turns a pod's random address into the pair's real MAC, so all state is
keyed by the real MAC ([state.md](state.md#per-pair)). Keyed by the advertised address, one
pair would split across an address per pod, and gain more as the addresses rotate - every
few minutes, and every time the lid closes.

**What cannot be identified is dropped, not kept.** Its rotating addresses cannot be
collapsed, nothing can be attributed to it, and storing it - an entry and a broadcast
each - is exactly what a BLE spam flood (a Flipper Zero or an ESP32 sending proximity
pairing messages) fed on. So the cache stays bounded by the stored keys. The keys arrive
with the pair's first AAP connection.

## Hardware checks

- **Byte 4's nibble order on a Pro 2 (USB-C):** its samples all have byte 4 = `0xaa`,
  which cannot tell the halves apart. As on the Pro 3
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#the-184-run),
  #184): wear the pods with 20 points or more between them, log `BLE parsable raw:`
  bytes 0-8 beside AAP's levels, take the primary pod out for a moment so the other
  sends, and note which nibble carries the sender's level.
