# BLE proximity pairing

The proximity pairing advertisement AirPods broadcast over BLE: its cleartext bytes,
its 16 encrypted bytes, and how they decrypt and validate. The layout started from
LibrePods' parser and has been checked here on two models only, AirPods Pro 3 and
AirPods Pro 2 (USB-C) (see [Runs](#runs)). Rows for other models come
from public tables and are unverified; several bytes are still unexplained (see
[Open questions](#open-questions)).

Parsed by `ble::parser` and `ble::decrypt`; what LinuxPods does with it:
[app/identification.md](../../app/identification.md).

**Byte offsets** count from the first payload byte, the one after the type and length
bytes: byte 0 is the prefix `0x01`. In the decrypted block, they count from its first
byte.

## Overview

AirPods broadcast their batteries, charging, in-ear, lid and connection state in BLE
advertisements, which any receiver nearby can read without a connection.

| Part | Bytes | What it holds | Needs |
|---|---|---|---|
| Cleartext | 0-8 | Model, status, lid, connection state, batteries in 10% steps | Nothing |
| Encrypted | 9-24 | Exact batteries, the last three bytes of the pair's real MAC | The pair's ENC_KEY, sent over AAP ([aap/keys.md](../aap/keys.md)) |

- **Identifying the pair needs both keys.** An advertisement comes from a resolvable
  private address that only the pair's IRK resolves ([addresses.md](addresses.md)),
  and only the pair's ENC_KEY decrypts it. Both keys come over AAP. What LinuxPods
  counts, and what it drops:
  [app/identification.md](../../app/identification.md#what-counts).
- **The payload changes seldom.** It changes when something happens, and a few times
  in the minute after; otherwise it stays the same for minutes
  ([Update Frequency](#update-frequency)).

## Advertisement Structure

### Manufacturer Data Format

The advertisement is Apple's entry in the manufacturer data: company ID `0x004C`,
Apple Inc. (Bluetooth SIG Assigned Numbers). Its value is:

| Offset | Field | Value |
|---|---|---|
| 0 | Message type | `0x07`, proximity pairing (furiousMAC/continuity, `messages/proximity_pairing.md`) |
| 1 | Payload length | `0x19` (25) in every payload seen here, on both models (7ba6bbc, 2026-09-09) |
| 2- | Payload | Byte 0 below |

### Payload Structure

| Byte | Field | Example | Details |
|---|---|---|---|
| 0 | Prefix | `0x01` | `0x01` in every payload captured here, on both models (`examples/decrypt_probe.rs`, #71) |
| 1-2 | Product ID, little-endian | `27 20` | [Bytes 1-2](#bytes-1-2-product-id) |
| 3 | Status | `0x0b` | [Byte 3](#byte-3-status-byte) |
| 4 | Pod batteries, a nibble each | `0x88` | [Byte 4](#byte-4-battery-levels-leftright-airpods) |
| 5 | Charging bits, case battery nibble | `0x07` | [Byte 5](#byte-5-charging-status) |
| 6 | Lid | `0x31` | [Byte 6](#byte-6-lid-state) |
| 7 | Color | `0x00` | [Byte 7](#byte-7-device-color) |
| 8 | Connection state | `0x04` | [Byte 8](#byte-8-connection-state) |
| 9-24 | Encrypted: exact batteries, MAC suffix | | [Bytes 9-24](#bytes-9-24-encrypted-battery-data) |

## Byte-by-Byte Parsing

### Which pod sent it

Each pod advertises as itself, from an address of its own, and calls itself "primary"
in its own advertisement (Pro 3, 2026-10-25, #71). Status bit 5 therefore names the
pod that sent it: `1` the left pod, `0` the right one. Its agreement with the primary
pod AAP reports is under [Update Frequency](#update-frequency).

The fields that carry one value per pod name the pods relative to the sender, so the
same bit means the left pod in one advertisement and the right pod in the next:

| Field | Left pod sends (bit 5 = 1) | Right pod sends (bit 5 = 0) | Source |
|---|---|---|---|
| Byte 4, low nibble | Left | Right | LibrePods' Linux parser; furiousMAC/continuity; one sample (Pro 3, 2026-09-11, 17f6591) |
| Byte 4, high nibble | Right | Left | As the low nibble |
| Byte 5, bit 4 (charging) | Left | Right | LibrePods' Linux parser |
| Byte 5, bit 5 (charging) | Right | Left | LibrePods' Linux parser |
| Status bit 1 (in ear), sender out of the case (bit 6 = 0) | Left | Right | LibrePods' Linux parser |
| Status bit 3 (in ear), sender out of the case (bit 6 = 0) | Right | Left | LibrePods' Linux parser |
| Status bit 1 (in ear), sender in the case (bit 6 = 1) | Right | Left | LibrePods' Linux parser |
| Status bit 3 (in ear), sender in the case (bit 6 = 1) | Left | Right | LibrePods' Linux parser |
| Decrypted byte 1 | Left | Right | Pro 3, 2026-10-25, #71 |
| Decrypted byte 2 | Right | Left | Pro 3, 2026-10-25, #71 |

In short: the sender has the low battery nibble, charging bit 4 and decrypted byte 1.
The case's fields do not depend on the sender.

- **Byte 4:** furiousMAC/continuity gives "the 4 MSBits are the right battery and the
  4 LSbits are the left battery", which is the left pod's order. In the one sample that
  tells them apart, the right pod sent byte 4 = `0x78` with the decrypted levels 80%
  for itself and 77% for the left pod: its own level is the low nibble's `8` (Pro 3,
  2026-09-11, 17f6591; [Runs](#september-samples)).
- **Decrypted bytes 1 and 2:** in the first #71 capture every level was 100%, but the
  charging bits settled it: in each advertisement from the right pod, decrypted byte 1
  was the right pod and byte 2 the left (Pro 3, 2026-10-25, #71).

### Bytes 1-2: Product ID

Apple's product ID, little-endian. It is the number BlueZ's `Modalias` carries for the
same pair: `p2027` for AirPods Pro 3, `p2024` for AirPods Pro 2 (USB-C) (2a4003d,
2026-10-16). Read big-endian, the bytes `27 20` give `0x2720`, a second number for one
model; LibrePods' table and furiousMAC/continuity's dissector list the IDs that way.

| Product ID | Model | Source |
|---|---|---|
| `0x2002` | AirPods (1st gen) | LibrePods and AirPodsDesktop agree |
| `0x200F` | AirPods (2nd gen) | LibrePods and AirPodsDesktop agree |
| `0x2013` | AirPods (3rd gen) | LibrePods and AirPodsDesktop agree |
| `0x2019` | AirPods 4 | LibrePods and AirPodsDesktop agree |
| `0x201B` | AirPods 4 (ANC) | LibrePods and AirPodsDesktop agree |
| `0x200E` | AirPods Pro | LibrePods and AirPodsDesktop agree |
| `0x2014` | AirPods Pro (2nd gen) | LibrePods and AirPodsDesktop agree |
| `0x2024` | AirPods Pro (2nd gen, USB-C) | Verified here |
| `0x2027` | AirPods Pro 3 | Verified here |
| `0x200A` | AirPods Max | LibrePods and AirPodsDesktop agree |
| `0x201F` | AirPods Max (USB-C) | LibrePods and AirPodsDesktop agree |

The tables are LibrePods' `linux/ble/blemanager.cpp` and AirPodsDesktop's
`Source/Core/AppleCP.cpp` (see [References](#references)); a model is listed only
where both agree (2a4003d). furiousMAC/continuity's dissector lists the first two
generations and the first AirPods Pro.

### Byte 3: Status Byte

The status byte says which pod sent the advertisement, which pods are in an ear, and
which are in the case.

| Bit | Meaning | Source |
|---|---|---|
| 0 | Unknown | |
| 1 | A pod is in an ear; which one: [Which pod sent it](#which-pod-sent-it) | LibrePods' Linux parser |
| 2 | Both pods are in the case | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 3 | The other pod's in-ear bit; see bit 1 | LibrePods' Linux parser |
| 4 | One pod is in the case, sent by the worn pod | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 5 | The sending pod: `1` left, `0` right | LibrePods' Linux parser; Pro 3, #71 and #142 |
| 6 | The sending pod is in the case | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 7 | Unknown | |

What the #71 captures (Pro 3, 2026-10-25) showed of bits 2, 4 and 6:

- **Bit 2:** with both pods in the case, every advertisement from either pod has it.
- **Bit 6:** with one pod in the case and the other worn, the pod in the case sets it.
  Not every pod in the case sets it: with both inside, only one pod at a time did.
- **Out of the case or in ears,** neither bit 2 nor bit 6 is set.
- **Bit 4:** the worn pod whose partner is in the case sets it (`0x13`, beside the
  partner's `0x73`). It is clear whenever no pod is in the case: `0x0b` both worn,
  `0x01` both on the desk. It is not set with every pod in the case. One capture
  contradicts it (see [Runs](#the-71-captures)).
- Bits 2 and 6 together gate the lid: see [Byte 6](#byte-6-lid-state).

#### Ear Detection

Bits 1 and 3 say which pods are in an ear, read as in
[Which pod sent it](#which-pod-sent-it). Each pod advertises its own status byte, so
two advertisements can disagree: with both pods worn, they flipped between left
out/right in and left in/right out within 0.6 s (Pro 3, 2026-11-01, #142). Where
LinuxPods takes in-ear from instead: [app/in-ear.md](../../app/in-ear.md).

### Byte 4: Battery Levels (Left/Right AirPods)

One nibble per pod: the sender's in the low nibble, the other pod's in the high one
([Which pod sent it](#which-pod-sent-it)). Each nibble is a level in 10% steps,
rounded down (see [Batteries in the clear and encrypted](#batteries-in-the-clear-and-encrypted)).

### Byte 5: Charging Status

| Bit | Meaning |
|---|---|
| 0-3 | Case battery, a nibble as in byte 4; `0xF` unknown |
| 4 | The sending pod is charging |
| 5 | The other pod is charging |
| 6 | The case is charging |
| 7 | Unknown |

The layout is LibrePods' Linux parser's; the #71 captures (Pro 3, 2026-10-25) agree
with it.

### Byte 6: Lid State

| Bits | Meaning | Source |
|---|---|---|
| 0-2 | Unknown; not a lid-open counter | Pro 3, 2026-10-25, #71 |
| 3 | Lid: `0` open, `1` closed, while a pod is in the case | LibrePods' Linux parser; Pro 3, 2026-10-25, #71 |
| 4-7 | Follow the pod and the case's charging; values below | Pro 3, 2026-11-01, #71 |

**Bit 3 is the lid only while a pod is in the case,** that is while status bit 6 or bit
2 is set. With the pods out, bit 3 reads 0 whatever the lid is doing. Read that way,
the lid came out right for about 9,000 advertisements across the two #71 captures but
one, a right-pod `0x31` 45 ms before the lid closed (Pro 3, 2026-10-25, #71, #114).
With one pod worn and the other in the open case, both advertise: the one in the case
reads the lid (`0x73`), the worn one does not (`0x13`).

**Closing the lid flips bit 3 and nothing else** in bytes 0-8: both pods went from
`0x31` (the right pod from `0x11`) to `0x39` within 22 ms (Pro 3, 2026-10-25, #71).
The pods then stop advertising ([Update Frequency](#update-frequency)), so a closed
lid is seen only in the last advertisements before they go silent.

**The high nibble** follows the pod and the case's charging (Pro 3, 2026-11-01, #71):

| High nibble | Sender |
|---|---|
| `0x1` | Out of the case, whether the case charges or not |
| `0x3` | In a case that is not charging |
| `0x5` | In a charging case |

The low bits are not a lid-open counter, which LibrePods calls them: they stayed `1`
across two lid opens and went to `2` when the pods went back in (Pro 3, 2026-10-25,
#71). The captures behind these rules: [Runs](#the-71-captures).

How LinuxPods reads and keeps the lid: [app/state.md](../../app/state.md#the-lid).

### Byte 7: Device Color

| Value | Color |
|---|---|
| `0x00` | White |
| `0x01` | Black |
| `0x02` | Red |
| `0x03` | Blue |
| `0x04` | Pink |
| `0x05` | Gray |
| `0x06` | Silver |
| `0x07` | Gold |
| `0x08` | Rose Gold |
| `0x09` | Space Gray |
| `0x0A` | Dark Blue |
| `0x0B` | Light Blue |
| `0x0C` | Yellow |

The table is LibrePods' Linux parser's. Every captured advertisement in
`examples/decrypt_probe.rs`, from both models verified here, carries `0x00`, White. The
rest is unverified.

How LinuxPods shows it: [app/ui.md](../../app/ui.md#device-color).

### Byte 8: Connection State

Whether a host is connected, and what the AirPods are doing on it. The values and
names are LibrePods' Linux parser's (`linux/ble/blemanager.h`):

| Value | State |
|---|---|
| `0x00` | Disconnected |
| `0x04` | Idle |
| `0x05` | Music |
| `0x06` | Call |
| `0x07` | Ringing |
| `0x09` | Hanging Up |
| `0xFF` | Unknown: LibrePods' own stand-in for no value, "representing null in the original" |

Some values come and go (Pro 3, 2026-10-25, #71):

- `0x08` for an advertisement or two while a connection is set up, before `0x04`. The
  first capture had `0x0c` there.
- `0x04` for one or two advertisements while the pods page the host they last used,
  after the lid opens or the pods leave the case, even when the page is rejected and
  nothing connects.

⚠️ **Byte 8 is the last cleartext byte;** everything from byte 9 on is encrypted. A
parser reading "connection state" from byte 9 reads ciphertext, and its output is
meaningless (3eb67a6, 2026-09-11). Take that field from the decrypted payload instead,
or treat it as unknown.

### Bytes 9-24: Encrypted Battery Data

The last 16 bytes hold the exact batteries, and the real MAC's suffix, which confirms
that the key was the pair's.

- **Algorithm:** AES-128 in ECB mode: a single block, no IV, no padding.
- **Key:** the ENC_KEY (key type `0x04`), which the pair sends over its AAP connection
  (L2CAP PSM 4097): [aap/keys.md](../aap/keys.md).
- **Where it is:** every payload seen here is exactly 25 bytes, so bytes 9-24 are also
  the last 16 bytes (7ba6bbc, 2026-09-09). Where it sits in a payload of another length
  is an [open question](#open-questions). Which range LinuxPods takes:
  [app/identification.md](../../app/identification.md#where-the-ciphertext-is).

Both models verified here decrypt this way (2026-09-09/10, 7ba6bbc, 1a7c66d). The tools
that decrypt captured and live payloads:
[development/probes.md](../../development/probes.md#decrypt_probe).

#### Decrypting BLE Advertisements

With the pair's ENC_KEY (key type `0x04`):

1. **Take** bytes 9-24 of the payload.
2. **Decrypt** them: AES-128 ECB, one block, no padding.
3. **Validate** the plaintext by its MAC suffix: [Decryption Validation](#decryption-validation).
4. **Read** the batteries from bytes 1-3: the sending pod, the other pod, the case.

`examples/decrypt_probe.rs` does this for captured payloads. Which keys LinuxPods
tries: [app/identification.md](../../app/identification.md#what-counts).

#### Decrypted Format

| Byte | Field | Source |
|---|---|---|
| 0 | Connection state, as cleartext byte 8; high bits unexplained | Pro 3, 2026-10-25, #71 |
| 1 | Sending pod: bit 7 charging, bits 0-6 the level | Levels: both models, 2026-09-09/10; which pod: Pro 3, 2026-10-25, #71 |
| 2 | Other pod: bit 7 charging, bits 0-6 the level | Levels: both models, 2026-09-09/10; which pod: Pro 3, 2026-10-25, #71 |
| 3 | Case: bit 7 charging, bits 0-6 the level | Both models, 2026-09-09/10 |
| 4-6 | The last three bytes of the address of the host the pods last used, connected or not | Pro 3, 2026-10-25, #71 |
| 7-9 | The last three bytes of the pair's real MAC, or `00 00 00` unless both pods are in the case | Both models, 2026-09-09/10; Pro 3, #71 |
| 10 | Unknown; `00` in every sample | Both models, 2026-09-09/10 |
| 11 | Counts up slowly | Pro 3, 2026-10-25, #71 |
| 12-15 | Unknown; change whenever the payload does | Pro 3, 2026-10-25, #71 |

- **Byte 0** equaled cleartext byte 8 in all 111 payloads of the second #71 capture:
  `00`, `04` and `08`. The September samples carry `0x10` on top on the Pro 3, and the
  first #71 capture a `0x40`.
- **Bytes 4-6** name the host the pods page. They changed when this host connected,
  and changed back 1.4 s after it disconnected (first #71 capture).
- **A level above 100** means the part is unavailable: with both pods worn, the
  decrypted case byte was `0xff` beside the cleartext case nibble `0xF` (Pro 3,
  2026-09-11, 17f6591).
- The samples behind this table: [Runs](#runs).

### Decryption Validation

AES always "succeeds": a wrong key gives garbage, not an error. So the plaintext must
be checked before it is trusted, and the check is its MAC suffix.

**The check.** Bytes 7-9 of the plaintext carry the last three bytes of the device's
real MAC. Compare them with the MAC the key is stored under:

```
valid = (plaintext bytes 7-9 == the MAC's last three bytes) or (plaintext bytes 7-9 == 00 00 00)
```

This holds on both models tested (2026-09-10, 1a7c66d). It also checks something
useful: the payload carries its own device's address, so a match says the key it was
decrypted with is that device's.

**Out of the case, the suffix is zeroed.** Unless both pods are in the case (status
bit 2), the pods send `00 00 00` in bytes 7-9, whether a host is connected or not. The
suffix went to zero with the first advertisement from out of the case, and came back
about 0.1 s after bit 2 when the pods went back in (Pro 3, 2026-10-25, #71). It was
first seen on a Pro 3 connected and out of the case, the rest of the plaintext intact
(2026-09-11, 17f6591); the sample is under [Runs](#runs). What rejecting that form would
do in LinuxPods: [app/identification.md](../../app/identification.md#the-suffix).

**Accepting both forms is safe.** The suffix only tests that the key was right, and
zeros test that just as well: a wrong key yields pseudorandom bytes either way. The
union of the two rules costs exactly one bit: a wrong key passes one time in 2^23
rather than 2^24. That was measured at 4 and 4 false accepts over 40M random keys
against one real ciphertext (17f6591, 2026-09-11).

**With the address resolved first** ([addresses.md](addresses.md#resolving-the-advertising-address)),
this is the only check of the plaintext needed.

⚠️ **Do not validate with magic bytes.** Earlier reverse engineering described a marker
of byte 0's upper nibble clear and byte 4 == `0x2D`. Neither model tested here produces
it (2026-09-10, 1a7c66d):

- byte 4 is part of the last host's address, whatever that host's address is;
- byte 0 follows the connection state, with `0x10` on top in the Pro 3 samples.

A magic-byte check therefore rejects *correct* decryptions, which leaves every pair
unidentified. Do not add one beside the suffix check either. What that does in
LinuxPods: [app/identification.md](../../app/identification.md#the-suffix).

## Accuracy Limitations

### Batteries in the clear and encrypted

| | Cleartext (bytes 4-5) | Encrypted (bytes 9-24) |
|---|---|---|
| Steps | 10%, rounded down (0%, 10%, ..., 100%) | 1% (0-100%), the levels AAP reports (source not recorded) |
| Encoding | A nibble: `0x0`-`0x9` = 0-90%, `0xA`-`0xE` = 100%, `0xF` = unknown | Bit 7 charging, bits 0-6 the level |
| Key needed | None | The ENC_KEY, from AAP ([aap/keys.md](../aap/keys.md)) |
| Says which pair sent it | No | Yes |

- **Rounded down:** the September samples (both models, 2026-09-09/10) advertised 58% as
  50% and 14% as 10% in the clear.
- **`0xA` = 100%:** both Pro 2 (USB-C) September payloads in
  `examples/decrypt_probe.rs` carry byte 4 = `0xaa` and decrypt to 100% / 100%
  (2026-09-10, 1a7c66d).
- **`0xB`-`0xE` = 100%:** from LinuxPods' first notes, after LibrePods at the time
  (3e5033a, 2025-10-12); never recorded here. LibrePods' Linux parser treats only
  `0xF` as special (`linux/ble/blemanager.cpp` at commit 89356c0).
- **The encrypted levels** tracked the cleartext ones on both models (2026-09-09/10,
  7ba6bbc).

Which levels LinuxPods shows: [app/identification.md](../../app/identification.md#batteries-in-the-clear-and-encrypted).

### Update Frequency

| What | Observed | Source |
|---|---|---|
| Advertisements | About 5 a second from each pod that advertises, in every steady state: open case, out of the case, connected or not | Pro 3, 2026-10-25, #71 |
| Bursts | 30-35 a second for about 10 s after the lid opens or the pods go back in the case | Pro 3, 2026-10-25, #71 |
| Payload | Repeated for hundreds of advertisements; changes in bursts when something happens, again about 5 s, 30 s and 60 s later, then stays | Pro 3 and Pro 2 (USB-C), 987ffe3 |
| Longest unchanged stretch | 163 s, pods worn, connected to this host or to nothing, on both models | Pro 3 and Pro 2 (USB-C), 987ffe3 |
| Idle pair | Same payload for minutes: over 40 s of scanning and across 2 minutes before it, in the open case, not connected to this host | Pro 3, 2026-11-02, #153 |
| Closed lid | The pods stop advertising about 4 s after the lid closes | Pro 3, 2026-10-25 and 2026-11-01, #71 |
| Discovery, or a monitor | When bluetoothd reports the pods' advertisements: [linux/bluez.md](../linux/bluez.md#discovery), and through an advertisement monitor [linux/bluez.md](../linux/bluez.md#discovery-and-directed-advertising) | |

The payload gaps of 25 and 30 s are most likely what was once recorded here as a
refresh every 20-30 s (a guess). On 2026-11-02, in each app run, each pod's payload
also changed once about 2 s after discovery started, then stayed (Pro 3, #153).

**Which pod advertises:**

- While either pod is in the case, both advertise, each from its own address (Pro 3,
  2026-10-25, #71).
- With both out of the case, worn or not, only the primary pod advertises: the right
  one in the first two #71 captures, where it was primary (Pro 3, 2026-10-25).
- In session 1 of #142, with both pods worn and the left primary at times, the
  advertisement's primary agreed with the primary pod AAP reported 13 times out of 13
  (Pro 3, 2026-11-01).

## Comparison: BLE vs AAP

### BLE Proximity Pairing (This Protocol)

| Feature | Cleartext | Encrypted |
|---|---|---|
| Battery steps | 10% | 1% |
| Identifies the pair | No | Yes (IRK, then ENC_KEY) |
| Update rate | On events | On events |
| Connection required | No | No, but the keys come over AAP |
| Works with an iPhone connected | Yes | Yes |
| Control (noise modes) | No | No |
| Keys required | No | IRK and ENC_KEY, from AAP |

What LinuxPods shows of each: [app/identification.md](../../app/identification.md#batteries-in-the-clear-and-encrypted).

### AAP (Apple Accessory Protocol)

| Feature | AAP |
|---|---|
| Battery | 1%, live |
| Update rate | Under 1 s (source not recorded) |
| Connection required | Yes, L2CAP PSM 4097 |
| Works with an iPhone connected | No: it disconnects the iPhone (source not recorded) |
| Control commands (noise modes, say) | Yes |

**Which to use:**

- **AAP** for live battery and control, while the AirPods are connected to this host.
- **BLE, encrypted,** for passive monitoring while they are connected elsewhere. The
  keys come once, over AAP.
- **BLE, cleartext,** cannot tell one pair from another behind their random addresses,
  so it is no basis for monitoring a pair. It can serve to offer pairing to AirPods
  nearby that no key identifies (#131).

The addresses the advertisements come from, and how the IRK resolves them:
[addresses.md](addresses.md).

## Runs

### September samples

Advertisements captured from the running app on 2026-09-09/10 from a Pro 3 and a
Pro 2 (USB-C), decrypted with each pair's key (7ba6bbc, 1a7c66d;
`cargo run --example decrypt_probe` decrypts the captured payloads). The real MACs
and the host address are placeholders:

```
Pro 3 (0x2027), real MAC AA:BB:CC:DD:EE:FF
10 be be 9f a1 a2 a3 [DD EE FF] 00 00 a7 8a b5 cc   -> 62% / 62% / 31%
10 c8 ca ba a1 a2 a3 [DD EE FF] 00 00 7e 5b ee 7a   -> 72% / 74% / 58%

Pro 2 (USB-C) (0x2024), real MAC 11:22:33:44:55:66
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 3b a2 15 bd   -> 100% / 100% / 14%
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 03 f4 13 e0   -> 100% / 100% / 14%
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        MAC suffix
   |        last host
   batteries
```

- Within one device, only bytes 1-3 (batteries) and 12-15 change between these samples.
- Bytes 4-6 are the same for both models because both had last used the same host.
- The suffix matched the real MAC on every sample with both pods in the case.
- The `0x2D` marker at byte 4 was not seen on either model.
- The cleartext `0x51`/`0x59` lid vectors of that month, from the Pro 3, all have the
  case charging (#71).

A Pro 3 connected and out of the case, 2026-09-11 (17f6591), the rest of the plaintext
intact:

```
14 50 4d ff a1 a2 a3 [00 00 00] 00 0f d4 65 7c 03   -> 80% / 77% / case unknown
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        zeroed suffix
   |        last host
   batteries
```

Its cleartext bytes 3-5 are `0b 78 8f`: the right pod sending (status bit 5 clear), both
pods worn, byte 4 = `0x78`, and the case nibble `0xF` (`examples/decrypt_probe.rs`).

The firmware recorded with the earliest runs here, 2025-10: 7A305 on the Pro 2 (USB-C)
(3e5033a), 8A353 on the Pro 3 (24ef39a).

### The #71 captures

AirPods Pro 3 on one adapter (#71):

| Capture | Date | Tools |
|---|---|---|
| 1 and 2 | 2026-10-25 | `btmon`, `dbus-monitor --system "sender='org.bluez'"` and the app at `RUST_LOG=linuxpods=trace`, build 06a426d |
| 4 | 2026-11-01 | The app at `RUST_LOG=linuxpods=trace`, build df29b14 |

- **Status byte, one pod in the case:** with both pods in the case, only one pod at a
  time set bit 6: the right pod's `0x15` beside the left's `0x75`, and later the left
  pod's `0x35` beside the right's `0x55`.
- **Status byte, bit 4:** the right pod's first advertisement after the lid opened was
  `0x04`, with both pods in the case and bit 4 clear. Being put back, the left pod sent
  `0x29`, `0x22` and `0x20` from the hand while the right pod already sent `0x51` from
  the case.
- **Byte 6 in the open case (capture 2):** the right pod sent `0x11` with the left
  pod's `0x31`, before both sent `0x32` after being put back. This does not fit the
  high-nibble table.
- **Byte 6 on the charger (capture 4):** both pods in the open case went from `0x32` to
  `0x52` half a second ahead of byte 5's charging bit (bit 6), and sent `0x5a` with the
  lid closed. The September `0x51`/`0x59` vectors, all with the case charging, fit.
- **Lid closed (capture 2):** both pods sent byte 6 = `0x39` within 22 ms of the marker
  and kept advertising for 3.9 s, then stopped.

### Payload changes

- 987ffe3: with the pods worn, connected to this host or to nothing, the payload
  changed again about 5 s, 30 s and 60 s after an event and then stayed; 163 s in the
  longest stretch, on a Pro 3 and a Pro 2 (USB-C). The dates of these runs are not
  recorded.
- #153, 2026-11-02: a Pro 3 idle in the open case, a 40 s LE scan
  (`bluetoothctl scan le`, D-Bus signals recorded). Each pod sent about 178
  `RSSI`-only `PropertiesChanged` and no `ManufacturerData` change. The payloads were
  the bytes the app had received 2 minutes earlier.

## References

- [LibrePods](https://github.com/librepods-org/librepods) - an open source Android and
  Linux AirPods client. Its Linux parser is `linux/ble/blemanager.cpp` and
  `linux/ble/blemanager.h`.
- [AirPodsDesktop](https://github.com/SpriteOvO/AirPodsDesktop) - its product IDs are in
  `Source/Core/AppleCP.cpp`.
- [OpenPods](https://github.com/adolfintel/OpenPods) - an open source Android AirPods
  client.
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity
  protocol documentation and a Wireshark dissector.
- BlueZ D-Bus API documentation.

## Open questions

- Status bits 0 and 7: unknown.
- Byte 5 bit 7: unknown.
- Byte 6 bits 0-2: what they count; they are not a lid-open counter.
- Byte 6: why the right pod sent `0x11` from the open case in capture 2 of #71.
- Byte 8: what `0x08` and `0x0c` mean during connection setup.
- Decrypted byte 0: what its high bits (`0x10`, `0x40`) mean. Do not validate against
  it.
- Decrypted byte 10: unknown; `00` in every sample.
- Decrypted byte 11: what it counts.
- Decrypted bytes 12-15: unknown; they change whenever the payload does, and may be a
  MIC (a guess).
- The payload length: every payload seen is 25 bytes. Whether a payload of another
  length keeps the encrypted bytes at byte 9 or at the end is unknown.
- Cleartext nibbles `0xB`-`0xE`: never recorded here.
- Status bit 4: being put back, the left pod sent `0x29`, `0x22` and `0x20` from the
  hand while the right pod already sent `0x51` from the case (capture 2 of #71), which
  the rule for bit 4 does not explain.
- Byte 4: which nibble carries the sender's level rests on one sample (Pro 3,
  2026-09-11, 17f6591).
- Why each pod's payload changed once about 2 s after discovery started (Pro 3,
  2026-11-02, #153). Under an advertisement monitor, with no active scan, the payloads
  changed within 0.5 s of each pod being first heard, one of them twice; under discovery
  right after, they did not
  ([linux/bluez.md](../linux/bluez.md#runs), runs C and C2, #189).
- Models other than the Pro 3 and the Pro 2 (USB-C): untested. The MAC-suffix rule and
  the byte-reversed IRK hold on both; older hardware is unknown.
  `examples/irk_probe.rs` checks a new model's addresses
  ([addresses.md](addresses.md#open-questions)).
- Color values other than `0x00` (White): unverified.
