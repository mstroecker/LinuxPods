# BLE proximity pairing

The proximity pairing advertisements AirPods broadcast: their cleartext bytes, the encrypted part, and how it decrypts.

This document describes the reverse-engineered Apple Continuity Proximity Pairing protocol used by AirPods to broadcast battery and status information via Bluetooth Low Energy (BLE) advertisements.

## Overview

AirPods continuously broadcast BLE advertisements containing battery levels, charging status, and device information. This allows nearby devices to display battery information **without establishing an active connection**.

### Key Characteristics

- **Passive Monitoring**: No connection required
- **Cleartext and encrypted parts**:
  - **Cleartext**: the model, status, lid and connection state, and the batteries in 10% steps
  - **Encrypted**: the exact batteries, and the last three bytes of the pair's real MAC. The
    key comes over AAP
- **Identification needs the keys**: an advertisement comes from a resolvable private address,
  which only the pair's IRK resolves, and only the pair's ENC_KEY decrypts it. Both keys come
  over AAP. What LinuxPods counts, and what it drops:
  [app/identification.md](../../app/identification.md#overview)
- **Slow Updates**: The payload changes when something happens, and a few times in the
  minute after; otherwise it stays the same, for minutes

## Advertisement Structure

### Manufacturer Data Format

```
Company ID: 0x004C (Apple Inc.)
Type: 0x07 (Proximity Pairing)
Length: Variable (typically 25 bytes)
```

### Payload Structure

The type and length bytes precede the payload; the parser slices them off, so byte
0 below is the first payload byte. Offsets are payload-relative throughout this
document, the code, and the `raw:` log lines.

```
Byte    Description                     Example     Status      Notes
----    -----------                     -------     ------      -----
type    Message Type                    0x07        ✅ Working   0x07 = proximity pairing
len     Length                          0x19        ✅ Working   Payload length (25 bytes)
0       Prefix                          0x01        ✅ Working   Always 0x01
1-2     Product ID (little-endian)      27 20       ✅ Working   0x2027 = AirPods Pro 3
3       Status Byte                     0x0b        ✅ Working   Ear detection, orientation, in case
4       Battery Levels                  0x88        ✅ Working   Pod batteries, a nibble each (10% steps)
5       Charging + Case Battery         0x07        ✅ Working   Charging bits, case battery nibble
6       Lid State                       0x31        ✅ Working   Bit 3 = lid (0=open), read while in case
7       Device Color                    0x00        ✅ Working   Color byte
8       Connection State                0x04        ✅ Working   0x00 disconnected, 0x04 idle, 0x05 music, ...
9-24    Encrypted Battery Data          ...         ✅ Working   AES-128 ECB: exact batteries, MAC suffix
```

**Working Features** (unencrypted): All batteries (10% steps), In Ear detection, Orientation (`is_flipped`), Model, Color, Lid state<br>
**Not Working** (format unknown): Byte 6 bits 0-2 and 4-7

## Byte-by-Byte Parsing

### Important: Orientation Handling

Each pod advertises as itself and calls itself "primary" (#71), so status bit 5 says which pod sent the advertisement: set by the left pod, clear from the right. In the right pod's advertisements several data fields are **swapped**:
- The primary is encoded in Byte 3 (Status)
- Battery level nibbles (left ↔ right)
- Charging status bits (left ↔ right)
- Ear detection bits (uses XOR logic)

Parse **byte 3 (status byte)** to determine orientation.

### Bytes 1-2: Product ID

Apple's product ID, little-endian - the number BlueZ's `Modalias` carries for the
same pair (`p2027` for AirPods Pro 3). Read big-endian, the bytes `27 20` give
`0x2720`, a second number for one model; several public tables list the IDs that
way. `decode_model_name` in `src/ble/parser.rs` names them:

```
Product ID  Device
----------  ------
0x2002      AirPods (1st gen)
0x200F      AirPods (2nd gen)
0x2013      AirPods (3rd gen)
0x2019      AirPods 4
0x201B      AirPods 4 (ANC)
0x200E      AirPods Pro
0x2014      AirPods Pro (2nd gen)
0x2024      AirPods Pro (2nd gen, USB-C)   verified here
0x2027      AirPods Pro 3                  verified here
0x200A      AirPods Max
0x201F      AirPods Max (USB-C)
```

Only the two marked are verified on hardware in this project. The rest are listed
where two public tables agree - the function's documentation names them.

**Decoding:**
```rust
let product_id = u16::from_le_bytes([payload[1], payload[2]]);
```

### Byte 3: Status Byte

Encodes device status flags including ear detection, orientation and which pods
are in the case:

```
Bit     Flag                    Example
---     ----                    -------
0       Unknown
1       In Ear (Primary)        1 = In Ear, 0 = Not In Ear
2       Both Pods In Case       1 = both in the case
3       In Ear (Secondary)      1 = In Ear, 0 = Not In Ear
4       One Pod In Case         1 = a pod is in the case (see below)
5       Primary Pod             0 = Right Primary, 1 = Left Primary
6       This Pod In Case        1 = the sending pod is in the case
7       Unknown
```

Bits 2, 4 and 6 are named after LibrePods' Linux parser, and agree with the
#71 captures (AirPods Pro 3): with both pods in the case every advertisement
from either pod has bit 2, and with one pod in the case and the other worn the
one in the case has bit 6. Out of the case or in ears neither is set. Bit 6 is
not set by every pod in the case: with both inside, only one pod at a time set
it - the right pod's `0x15` beside the left's `0x75`, and later the left pod's
`0x35` beside the right's `0x55`. Bits 2 and 6 together gate the lid; see
[Byte 6](#byte-6-lid-state).

Bit 4 is set by the worn pod whose partner is in the case (`0x13`, beside the
partner's `0x73`), and is clear whenever no pod is in the case: `0x0b` both
worn, `0x01` both on the desk. It is not set with every pod in the case - the
right pod's first advertisement after the lid opened was `0x04` - and one
capture contradicts it: being put back, the left pod sent `0x29`, `0x22` and
`0x20` from the hand while the right pod already sent `0x51` from the case.
Only the first matters to the lid, below.

**Primary Pod Determination:**

Each pod calls itself "primary" in its own advertisement, so the primary flag names the pod that sent it. It decides how battery levels, charging status, and ear detection should be interpreted:

```rust
let is_flipped = !primary_left;
let xor_factor = primary_left != this_in_case;
```

- **`is_flipped`**: When `true`, battery nibbles and charging bits are swapped
- **`xor_factor`**: Used to determine correct ear detection bits

#### Ear Detection

✅ **Working, with limits** - detects when AirPods are in/out of ears
- Each pod advertises its own status byte: with both pods worn, the advertisements
  flipped between `in_ear=false/true` and `true/false` within 0.6 s (#142)
- [app/in-ear.md](../../app/in-ear.md)

### Byte 4: Battery Levels (Left/Right AirPods)

A nibble per pod, the primary pod's in the high one:
```
Bit     Component (Normal)
---     ------------------
0-3     Right AirPod Battery
4-7     Left AirPod Battery
---     ------------------
```

With the right pod primary the two swap. Each nibble is a level in 10% steps,
rounded down - see [Accuracy Limitations](#accuracy-limitations).

### Byte 5: Charging Status

Encodes charging state for all three components and the case battery.
**Bits 4 and 5 are swapped when the right pod is primary:**

```
Bit     Component (Normal)
---     ------------------
0-3     Case Battery (nibble, as byte 4; 0xF = unknown)
4       Left AirPod Charging
5       Right AirPod Charging
6       Case Charging
7       Unknown
```

### Byte 6: Lid State

```
Bit     Meaning
---     -------
0-2     Unknown
3       Lid state (0 = open, 1 = closed)
4-7     Unknown
```

✅ **Working** - `lid_open` reads bit 3, while the status byte puts a pod in the
case:

```rust
let in_case = status & 0x44 != 0; // bit 6: this pod, bit 2: both pods
let lid_open = in_case.then(|| payload[6] & 0x08 == 0);
```

> **Note:** bit 3 only means anything while a pod is in the case. With them out,
> it reads 0 whatever the lid is doing, so `lid_open` is `None` unless status bit
> 6 or 2 is set.

With one pod worn and the other in the open case, both advertise: the one in
the case reads the lid (`0x73`), the worn one doesn't (`0x13`).

How the app keeps the lid across the two: [app/state.md](../../app/state.md#the-lid).

Closing the lid flips bit 3 and nothing else in bytes 0-8: in the #71 capture
both pods went from `0x31` (the right pod from `0x11`) to `0x39` within 22 ms.
About 4 s later the pods stop advertising, so a closed lid is only seen in the
last advertisements before they go silent.

How long the app shows a closed lid: [app/state.md](../../app/state.md#the-lid).

The high nibble follows the pod and the case's charging, as capture 4 of #71
settled: `0x1` from a pod out of the case, whether the case charges or not;
`0x3` from one in a case that isn't charging; `0x5` from one in a charging case.
Put on the charger, both pods in the open case went from `0x32` to `0x52` half
a second ahead of byte 5's charging bit (bit 6), and with the lid closed they
sent `0x5a`. The September `0x51`/`0x59` vectors, all with the case charging,
fit. One capture does not fit yet: in capture 2 the right pod sent `0x11` from
the open case, with the left pod's `0x31`, before both sent `0x32` after being
put back. The parser does not read the nibble. The low bits are not a lid-open
counter: they stayed `1` across two lid opens and went to `2` when the pods went
back in.

### Byte 7: Device Color

✅ **Working** - `decode_color` in `src/ble/parser.rs` names it. The captured
advertisements in `examples/decrypt_probe.rs`, from the two models verified
here, all carry `0x00`, White; the rest of the table is unverified:

```
Color byte -> name
0x00: White
0x01: Black
0x02: Red
0x03: Blue
0x04: Pink
0x05: Gray
0x06: Silver
0x07: Gold
0x08: Rose Gold
0x09: Space Gray
0x0A: Dark Blue
0x0B: Light Blue
0x0C: Yellow
```

How the app shows it: [app/ui.md](../../app/ui.md#device-color).

### Byte 8: Connection State

✅ **Working** - whether a host is connected, and what the AirPods are doing on it.
See [Connection State](#connection-state) for the values `decode_connection_state`
names. The #71 captures (AirPods Pro 3) add some that come and go:

- `0x08` for an advertisement or two while a connection is set up, before `0x04`;
  the first capture had `0x0c` there
- `0x04` for one or two advertisements while the pods page the host they last
  used - after the lid opens, or the pods leave the case - even when the page is
  rejected and nothing connects

⚠️ Byte 8 is the **last cleartext byte**; everything from byte 9 on is encrypted.
Any parser reading "connection state" from byte 9 is reading ciphertext, and its
output is meaningless. Take that field from the decrypted payload instead, or
treat it as unknown.

#### Connection State

See `decode_connection_state` in `src/ble/parser.rs`:

```
Value  State
-----  -----
0x00   Disconnected
0x04   Idle
0x05   Music
0x06   Call
0x07   Ringing
0x09   Hanging Up
0xFF   Unknown
```

### Bytes 9-24: Encrypted Battery Data

✅ **Working** - The final 16 bytes hold the exact batteries, and the real MAC's
suffix, which confirms the key was the pair's.

**Decryption Details:**
- **Algorithm**: AES-128 ECB mode (single block, no IV, no padding)
- **Key Source**: Retrieved via AAP connection (PSM 4097) (See [AAP Key Retrieval](../aap/keys.md))
- **Key Type**: ENC_KEY from proximity pairing keys

The tools that decrypt captured and live payloads: [development/probes.md](../../development/probes.md#testing).

#### Decrypting BLE Advertisements

Once you have the **ENC_KEY** (type 0x04), you can decrypt BLE proximity pairing advertisements:

1. **Extract encrypted portion**: bytes 9-24 of the BLE payload
2. **Decrypt**: AES-128 ECB, single block, no padding
3. **Validate**: bytes 7-9 hold the last three bytes of the device's real MAC, or
   `00 00 00` unless both pods are in the case
4. **Parse**: bytes 1-3 are first pod, second pod and case (bit 7 = charging)

```rust
use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};

let cipher = Aes128::new_from_slice(enc_key)?;
let mut block = Block::<Aes128>::try_from(encrypted)?;
cipher.decrypt_block(&mut block);
// block now holds the 16-byte plaintext
```

AES always "succeeds", so step 3 is what tells a correct key from a wrong one -
because the payload carries its own MAC.

Which keys LinuxPods tries: [app/identification.md](../../app/identification.md#solution-identification-by-irk-then-enc_key).

**Decrypted Format** (16 bytes):
```
Byte    Description                     Status      Notes
----    -----------                     ------      -----
0       Connection state + flags        ❓          Cleartext byte 8; high bits unexplained
1       First Pod Battery + Charging    ✅ Working   Bit 7=charging, bits 0-6=level (exact)
2       Second Pod Battery + Charging   ✅ Working   Bit 7=charging, bits 0-6=level (exact)
3       Case Battery + Charging         ✅ Working   Bit 7=charging, bits 0-6=level (exact)
4-6     Last host's address suffix      ❓          The host the pods last used, connected or not
7-9     Real MAC suffix                 ✅ Working   Last 3 bytes of the permanent MAC; 00 00 00 unless both pods are in the case
10      Unknown                         ❓          00 in every sample
11      Counter                         ❓          Counts up slowly
12-15   Tail                            ❓          Changes whenever the payload does (MIC?)
```

The #71 captures (AirPods Pro 3) explain three of these:

- **Byte 0** equaled cleartext byte 8 in all 111 payloads of the second capture:
  `00`, `04` and `08`. The September samples below carry `0x10` on top on Pro 3,
  and the first capture a `0x40`; neither is explained.
- **Bytes 4-6** are the last three bytes of the address of the host the pods last
  used - the one they page - whether or not it is connected. They changed when
  another host connected, and changed back after it disconnected (#116).
- **Byte 11** counted up slowly across the first capture.

**Observed samples** (two models; the MACs and the host address are placeholders):
```
Pro 3 (0x2027), real MAC AA:BB:CC:DD:EE:FF
10 be be 9f a1 a2 a3 [DD EE FF] 00 00 a7 8a b5 cc   -> 62% / 62% / 31%
10 c8 ca ba a1 a2 a3 [DD EE FF] 00 00 7e 5b ee 7a   -> 72% / 74% / 58%

Gen 2 (0x2024), real MAC 11:22:33:44:55:66
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 3b a2 15 bd   -> 100% / 100% / 14%
00 e4 e4 8e a1 a2 a3 [44 55 66] 00 00 03 f4 13 e0   -> 100% / 100% / 14%
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        MAC suffix
   |        last host
   batteries
```
Within one device, only bytes 1-3 (batteries) and 12-15 (tail) change between
these samples. Bytes 4-6 are the same for both models because both had last used
the same host.

**Decryption Validation:**

⚠️ **Do not validate with magic bytes.** Earlier reverse engineering described a
marker of byte 0 upper-nibble clear plus byte 4 == `0x2D`. Neither model tested here
produces it: byte 4 is part of the last host's address, whatever that host's is,
and byte 0 follows the connection state, with `0x10` on top in the Pro 3 samples.
A magic-byte check therefore rejects *correct* decryptions, which leaves every pair
unidentified.

What that does in LinuxPods: [app/identification.md](../../app/identification.md#decryption-validation).

Validate against the **MAC suffix** instead - see [Decryption Validation](#decryption-validation) below.

This is both more reliable and more useful: the payload carries *its own device's*
address, so it checks that the key it was decrypted with is that device's.

What else LinuxPods checks: [app/identification.md](../../app/identification.md#decryption-validation).

**Orientation Handling:**
- If NOT flipped (left pod primary): Byte 1=left, Byte 2=right
- If flipped (right pod primary): Byte 1=right, Byte 2=left

**Battery Validation:**
- Values > 100 indicate unavailable/unknown battery

**Where it is:** Every payload observed so far is exactly 25 bytes, where bytes 9-24
are also the last 16 bytes. Whether a payload of another length would keep the
encrypted portion at byte 9 or at the end is unknown.

Which range LinuxPods takes: [app/identification.md](../../app/identification.md#where-it-is).

### Decryption Validation

Wrong keys produce garbage, but AES always "succeeds", so the decrypted payload
must be checked before it is trusted.

**The check - MAC suffix (confirmed on both models tested):**

Bytes 7-9 of the decrypted payload carry the last 3 bytes of the device's real
MAC. Compare them against the MAC the candidate key is stored under:

```rust
fn matches_device(decrypted: &[u8; 16], mac_addr: bluer::Address) -> bool {
    decrypted[7..10] == mac_addr[3..] || decrypted[7..10] == [0x00; 3]
}
```

**Out of the case, the suffix is zeroed.** The pods publish `00 00 00` in bytes
7-9 instead of the MAC suffix unless both are in the case (status bit 2), whether
a host is connected or not. In the #71 captures the suffix went to zero with the
first advertisement from out of the case, and came back about 0.1 s after bit 2
when the pods went back in. Observed on AirPods Pro 3 (0x2027), connected and
out of the case, the rest of the plaintext intact:

```
14 50 4d ff a1 a2 a3 [00 00 00] 00 0f d4 65 7c 03   -> 80% / 77% / case unknown
   ^^ ^^ ^^ ^^ ^^ ^^ ^^^^^^^^^^ ^^^^^
   |        |        |          bytes 10-11
   |        |        zeroed suffix
   |        last host
   batteries
```

What rejecting that form would do in LinuxPods: [app/identification.md](../../app/identification.md#decryption-validation).

**Why accepting both is safe.** The suffix is only the test that the
key was right, and zeros test that just as well - a wrong key yields pseudorandom
bytes either way. The union of the two
rules costs exactly one bit: 2^-23 per wrong key rather than 2^-24, measured at 4
and 4 false accepts respectively over 40M random keys against one real ciphertext.

With the address resolved first, this is the only check of the plaintext needed. Do
not add a magic-byte check alongside it - see the warning under
[Bytes 9-24](#bytes-9-24-encrypted-battery-data).

## Accuracy Limitations

### Batteries in the clear and encrypted

**Unencrypted Data** (bytes 4-5):
- **Granularity:** 10% steps, rounded down (0%, 10%, 20%, ..., 100%)
- **Encoding:** Nibble-based (0x0-0x9 = 0-90%, 0xA-0xE = 100%, 0xF = unknown)
- **No encryption key required** - but nothing in the clear says which pair sent it

**Encrypted Data** (bytes 9-24):
- **Granularity:** 1% (0-100%), the levels AAP reports
- **Encoding:** Bit 7 = charging, bits 0-6 = level
- **Requires ENC_KEY from AAP connection (See [AAP Key Retrieval](../aap/keys.md))**

Which levels LinuxPods shows: [app/identification.md](../../app/identification.md#batteries-in-the-clear-and-encrypted).

### Update Frequency

From the #71 captures and the hardware checks since (AirPods Pro 3, and Pro 2
USB-C where noted):

- **Advertisements:** about 5 a second from each pod that advertises, in every
  steady state - open case, out of the case, connected or not - with bursts of
  30-35 a second for about 10 s after the lid opens or the pods go back in the case
- **Payload:** repeated for hundreds of advertisements. It changes in bursts when
  something happens, and again about 5 s, 30 s and 60 s later with nothing else
  changing, then stays: 163 s in the longest stretch, the pods worn, connected to
  this host or to nothing, on both models. Those gaps of 25 and 30 s are most
  likely what was once recorded here as a refresh every 20-30 s. On 2026-11-02 a
  pair idle in the open case, connected to a phone, sent the same payload for
  minutes, over 40 s of scanning and across 2 minutes before it (#153). In each
  app run that day, each pod's payload also changed once about 2 s after
  discovery started, then stayed; the cause is unknown
- **Discovery:** when bluetoothd reports the pods' advertisements - [protocol/linux/bluez.md](../linux/bluez.md#discovery)
- **Closed lid:** the pods stop advertising about 4 s after the lid closes
- **Which pod:** while either pod is in the case, both advertise, each from its
  own address. With both out of the case, worn or not, only the primary does:
  the right one in captures 1 and 2, where it was primary, and in session 1 of
  #142, both pods worn and the left primary at times, the advertisement's
  primary agreed with AAP's 13 times out of 13

## Comparison: BLE vs AAP

### BLE Proximity Pairing (This Protocol)

| Feature | Unencrypted | Encrypted |
|---------|-------------|-----------|
| Battery Granularity | 10% steps | 1% |
| Identifies the pair | No | Yes (IRK, then ENC_KEY) |
| Update Rate | on events | on events |
| Connection Required | No | No (but needs key from AAP) |
| Works with iPhone connected | Yes | Yes |
| Control (Noise modes) | No | No |
| Keys Required | No | Yes: IRK and ENC_KEY, from AAP |

What LinuxPods shows of each: [app/identification.md](../../app/identification.md#comparison-ble-vs-aap).

### AAP (Apple Accessory Protocol)

| Feature                             | Status |
|-------------------------------------|--------|
| Battery                             | 1%, live |
| Update Rate                         | <1s |
| Connection Required                 | Yes (L2CAP PSM 4097) |
| Works with iPhone connected         | No (disconnects iPhone) |
| Control Commands (e.g. Noise modes) | Yes |

**Recommendation:**
- Use **AAP** for live battery and control when the AirPods are connected to Linux
- Use **BLE (encrypted)** for passive monitoring while they are connected elsewhere
  (requires one-time key retrieval via AAP)
- **BLE (unencrypted)** cannot tell one pair from another behind their random
  addresses, so it is no basis for monitoring a pair. What it can serve is offering
  to pair AirPods nearby that no key identifies (#131)

The addresses the advertisements come from, and how the IRK resolves them: [addresses.md](addresses.md).

## References

- [LibrePods](https://github.com/librepods-org/librepods) - Open source Android and Linux AirPods client
- [OpenPods](https://github.com/adolfintel/OpenPods) - Open source Android AirPods client
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity protocol documentation
- BlueZ D-Bus API documentation

## Known Limitations

1. **Update latency** - the payload changes on events, and otherwise stays the same
   for minutes (inherent to protocol)
2. **Cleartext batteries** - 10% steps, and nothing to tell which pair sent them
3. **Key requirement** - identifying a pair, and with it any BLE reading in LinuxPods,
   requires one-time retrieval of its IRK and ENC_KEY via AAP connection
4. **Byte 0's high bits are unexplained** - the rest of it follows the connection
   state. Do not validate against it. Validate by MAC suffix (bytes 7-9); see
   [Decryption Validation](#decryption-validation)
5. **Bytes 10 and 12-15 unidentified** - byte 10 is `00` in every sample; bytes 12-15
   change whenever the payload does, and may be a MIC. Neither has been confirmed
6. **Only two models tested** - AirPods Pro 3 (0x2027) and AirPods Pro Gen 2 (0x2024).
   The MAC-suffix rule and IRK resolution, with the IRK byte-reversed, hold on both;
   behavior on older hardware is unknown. `examples/irk_probe.rs` checks a new model:
   it reports an address that resolves only with the IRK unreversed, which the app
   would miss

---

**Last Updated:** 2026-11-02<br>
**Tested With:**
 - AirPods Pro (Gen 2) (0x2024), Firmware 7A305
 - AirPods Pro 3 (0x2027), Firmware 8A353

**Confidence note:** The decrypted layout was derived from advertisements captured
from two devices on 2026-09-09/10. Byte positions 1-3 (batteries) and 7-9 (MAC
suffix) are confirmed on both - the batteries track the cleartext values, and the
suffix matched the real MAC on every sample with both pods in the case. Bytes 0, 4-6
and 11 are read from the two #71 captures of AirPods Pro 3; the rest is unexplained.
The `0x2D` marker at byte 4 described by earlier reverse engineering was not
observed on either device.
