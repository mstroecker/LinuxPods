# Identification

Which advertisements LinuxPods counts, how it ties each to a pair, and what it drops.

## Overview

LinuxPods counts an advertisement only if its address resolves with a stored IRK
and that pair's ENC_KEY decrypts it, and drops every other, so what it shows over BLE is
always the exact, decrypted reading - see
[Device Identification](#solution-identification-by-irk-then-enc_key). The cleartext is what
an unidentified pair advertises, and is still decoded: offering such a pair for pairing
would read it (#131).

The keys, and what they open: [protocol/aap/keys.md](../protocol/aap/keys.md#why-this-matters).

## Solution: Identification by IRK, then ENC_KEY

How an address resolves, and how often the two checks agreed on hardware: [protocol/ble/addresses.md](../protocol/ble/addresses.md#resolving-the-advertising-address).

LinuxPods requires both:

1. **Store the keys by real MAC**: The key packet carries the IRK (type `0x01`) and the
   ENC_KEY (type `0x04`). Both are stored under the AAP connection's MAC address, on
   every connection - see [AAP Key Retrieval](../protocol/aap/keys.md)
2. **Resolve the address** with the pair's IRK - see
   [Resolving the Advertising Address](../protocol/ble/addresses.md#resolving-the-advertising-address)
3. **Decrypt with that pair's ENC_KEY only**, and check the decrypted MAC suffix - see
   [Decryption Validation](../protocol/ble/proximity-pairing.md#decryption-validation)
4. **Identify device**: An address that resolves, with a payload that pair's key
   decrypts, identifies the real device. Anything else is dropped

LinuxPods tries only the key of the pair
whose IRK resolved the address.

**Why both.** With the ENC_KEY alone, every stored key was tried on every
advertisement, and a wrong key passes the suffix check one time in 2^23 (see
[Decryption Validation](../protocol/ble/proximity-pairing.md#decryption-validation)).
That chance was paid per advertisement and per key: with about ten foreign pairs in
range and two stored keys, one false match about every 12 hours; a spammer sending
random payloads at 1000 a second, one every couple of hours per key. A false match
gave one of the user's pairs random levels, cached for 30 minutes. A wrong IRK's
chance of resolving an address is paid once per address, not per advertisement
([the odds](../protocol/ble/addresses.md#false-matches)) - and the resolving pair's key,
the only one tried, still has to pass the suffix check. A recorded advertisement replayed from another address is dropped,
too.

**Cost.** A resolve is one AES block per stored IRK. The coordinator remembers the
addresses that resolved, and which pair each is, for 10 minutes (longer than an
address lasts), at most 64 of them, the oldest going first: an address is resolved
about once. An address no IRK resolves is not remembered, as a spammer rotating its
address would only churn the list; it is resolved again with each advertisement,
which costs what trying every key on it used to.

**Pairs keyed before the IRK was kept.** The keystore kept only the ENC_KEY until
format version 2. A pair keyed then has no IRK until its next AAP connection brings
one, as the keys are requested on every connection. Until then it is identified by its
key alone, as before: its key is tried on every advertisement no stored IRK resolves.

## Decryption Validation

With a magic-byte check, which leaves every pair unidentified
([the warning](../protocol/ble/proximity-pairing.md#decryption-validation)):
LinuxPods drops unidentified advertisements, so nothing would show over BLE at all.

The MAC suffix is the only check of the plaintext this implementation performs; the
address the advertisement comes from has to resolve with the same pair's IRK first - see
[Device Identification](#solution-identification-by-irk-then-enc_key).

Rejecting the zeroed suffix ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decryption-validation))
leaves the pair unidentified whenever a pod is out of the case - in someone's ears,
connected to an iPhone or not - and an unidentified advertisement is dropped, so the
pair would vanish from BLE in exactly the case LinuxPods sets out to support.

Identification never came from *reading* the suffix; it comes from the IRK that
resolved the address, and from which key produced the plaintext, one key being tried
at a time.

## Where it is

**Where it is:** LinuxPods takes the fixed range `payload[9..25]`
(`ENCRYPTED_RANGE` in `src/ble/parser.rs`), right after the last cleartext byte.

Where the encrypted portion sits in the payloads seen: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#bytes-9-24-encrypted-battery-data).

## Batteries in the clear and encrypted

The two kinds of level: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#batteries-in-the-clear-and-encrypted).

LinuxPods parses the cleartext levels, then replaces them with the decrypted ones.
An advertisement not identified as a pair's - its address resolved by a stored IRK,
then decrypted by that pair's key - is dropped, as it cannot be attributed to a
pair, so today the cleartext levels are never shown: what the keys give is not
precision but identification. The cleartext is still decoded, as it is all an
unidentified pair advertises - what offering such a pair for pairing would read
(#131).

The parser reads the sending pod's level from byte 4's high nibble. The one sample
that tells the nibbles apart puts it in the low nibble
([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#which-pod-sent-it)),
so the cleartext levels may come out swapped; nothing shows them today. See
[Hardware checks](#hardware-checks).

## Comparison: BLE vs AAP

The rest of the comparison: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#comparison-ble-vs-aap).

| Feature | Unencrypted | Encrypted |
|---------|-------------|-----------|
| Shown by LinuxPods | No | Yes |

## Multi-Device Workflow

The workflow, step by step: [protocol/ble/addresses.md](../protocol/ble/addresses.md#multi-device-workflow).

Because identification resolves the randomized MAC back to the permanent one,
per-device state should be keyed by the **real** MAC. Keying by the advertised
MAC instead splits one pair across an address per pod, and adds more as the
addresses rotate - every few minutes, and every time the lid closes.
Advertisements that cannot be identified - an address no stored IRK resolves, or a
key that does not match - are best dropped rather than kept: their rotating MACs
cannot be collapsed, nothing can be attributed to them, and storing them is exactly
what a BLE spam flood (a Flipper Zero or ESP32 broadcasting proximity pairing
messages) feeds on. LinuxPods keeps only what it identifies; the keys arrive on the
device's first AAP connection. What it keeps, it keeps for a day, and the levels,
charging and model across a relaunch too, in `readings.json` beside the keys (#170).

## Hardware checks

- **Which nibble of byte 4 carries the sender's level:** wear a Pro 3 with the pods
  ~30 points apart, log `BLE parsable raw:` beside the decrypted bytes 1-2 and AAP's
  battery, and note which nibble carries the sender's level.
