# Apple Continuity BLE Proximity Pairing Protocol

This document describes the reverse-engineered Apple Continuity Proximity Pairing protocol used by AirPods to broadcast battery and status information via Bluetooth Low Energy (BLE) advertisements.

## Overview

AirPods continuously broadcast BLE advertisements containing battery levels, charging status, and device information. This allows nearby devices to display battery information **without establishing an active connection**.

### Key Characteristics

- **Passive Monitoring**: No connection required
- **Two-Tier Accuracy System**:
  - **Unencrypted data**: ~10% battery accuracy (10% granularity)
  - **Encrypted data**: 1% battery accuracy (requires encryption key from AAP)
- **Slow Updates**: Advertisements update infrequently. Updates mainly take place when something happens.

### Use Cases

- Monitoring AirPods battery while connected to another device (e.g., iPhone)
- Low-power battery monitoring without establishing L2CAP connection
- Fallback when AAP (Apple Accessory Protocol) connection is unavailable

## Advertisement Structure

### Manufacturer Data Format

```
Company ID: 0x004C (Apple Inc.)
Type: 0x07 (Proximity Pairing)
Length: Variable (typically 25 bytes)
```

### Payload Structure

```
Byte    Description                     Example     Status      Notes
----    -----------                     -------     ------      -----
0       Prefix                          0x01        ✅ Working   Always 0x01
1-2     Device Model (Big-Endian)       0x2420      ✅ Working   0x2420 = AirPods Pro
3       Status Byte                     0x0b        ✅ Working   Ear detection, orientation
4       Battery Levels                  0x88        ✅ Working   Left/Right AirPods (~10% accuracy)
5       Charging + Case Battery         0x07        ✅ Working   Charging bits, case battery (~10% accuracy)
6       Lid Open Counter                0x08        ❌ TO FIX    Unknown format
7       Device Color                    0x00        ✅ Working   Color byte
8       Lid/Connection (encrypted?)     0x05        ❌ TO FIX    Encrypted, format unknown
9-24    Encrypted Battery Data          ...         ✅ Working   AES-128 ECB, 1% accuracy (if key available)
```

**Working Features** (unencrypted): All batteries (~10%), In Ear detection, Orientation (IsFlipped), Model, Color<br>
**Not Working** (encrypted/unknown format): Lid status, Connection state

## Byte-by-Byte Parsing

### Important: Orientation Handling

AirPods broadcast which pod is "primary" (left or right). When the right pod is primary, several data fields are **swapped**:
- The primary is encoded in Byte 3 (Status)
- Battery level nibbles (left ↔ right)
- Charging status bits (left ↔ right)
- Ear detection bits (uses XOR logic)

Parse **byte 3 (status byte)** to determine orientation.

### Bytes 1-2: Device Model

16-bit big-endian value identifying the AirPods model:

```
Model ID    Device
--------    ------
0x2420      AirPods Pro
0x0e20      AirPods Pro (older)
0x0220      AirPods (2nd gen)
0x2420      AirPods Pro (2nd gen)
0x2720      AirPods Pro 3
```

**Decoding:**
```go
deviceModel := uint16(payload[1])<<8 | uint16(payload[2])
```

### Byte 3: Status Byte

Encodes device status flags including ear detection and orientation:

```
Bit     Flag                    Example
---     ----                    -------
0       Unknown
1       In Ear (Primary)        1 = In Ear, 0 = Not In Ear
2       Unknown
3       In Ear (Secondary)      1 = In Ear, 0 = Not In Ear
4       Unknown
5       Primary Pod             0 = Right Primary, 1 = Left Primary
6       In Case                 1 = In Case, 0 = Not In Case
7       Unknown
```

**Primary Pod Determination:**

The AirPods broadcast which pod is "primary". This affects how battery levels, charging status, and ear detection should be interpreted:

```go
isFlipped := !primaryLeft
xorFactor := primaryLeft != thisInCase  // XOR operation
```

- **isFlipped**: When `true`, battery nibbles and charging bits are swapped
- **xorFactor**: Used to determine correct ear detection bits

**Note:** Ear detection may require calibration and may not work reliably in all scenarios.

### Byte 4: Battery Levels (Left/Right AirPods)

Battery levels for both AirPods are encoded using the same nibble system:
```
Bit     Component (Normal) 
---     ------------------
0-4     Left AirPod Battery
5-7     Right AirPod Battery
---     ------------------
```

Left and Right AirPods may be swapped based on the primary pod.

**Important:** These values are **approximate** and may differ from actual battery levels by 5-10%. The BLE advertisements update slowly and do not reflect real-time battery drainage.

### Byte 5: Charging Status

Encodes charging state for all three components. **Bits 2 and 3 are swapped based on orientation:**

```
Bit     Component (Normal)
---     ------------------
0       Unknown
1       Case Charging
2       Right AirPod Charging
3       Left AirPod Charging
4-7     Battery Case (May be > 100) => Unknown Battery
```

Left and Right AirPods may be swapped based on the primary pod.

### Byte 6: Unknown (Lid Open Counter?)

❌ **TO FIX** - Format unknown, appears to increment on lid events but exact encoding unclear.

### Byte 7: Device Color

✅ **Working** - See color decoding in `internal/ble/parser.go`:

```go
// DecodeColor maps color byte to readable name
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

### Byte 8: Lid/Connection Status (Encrypted?)

❌ **TO FIX** - This byte appears to contain lid and/or connection state but is likely encrypted or uses an unknown encoding. Current parsing attempts are unreliable.

⚠️ Byte 8 is the **last cleartext byte**; everything from byte 9 on is encrypted.
Any parser reading "connection state" from byte 9 is reading ciphertext, and its
output is meaningless. Take that field from the decrypted payload instead, or
treat it as unknown.


### Bytes 9-24: Encrypted Battery Data

✅ **Working** - The final 16 bytes contain encrypted battery data with 1% accuracy.

**Decryption Details:**
- **Algorithm**: AES-128 ECB mode (single block, no IV, no padding)
- **Key Source**: Retrieved via AAP connection (PSM 4097) (See [AAP Key Retrieval](aap-key-retrieval.md))
- **Key Type**: ENC_KEY from proximity pairing keys
- **Tools**:
  - `cmd/debug_aap_key_retrieval` - Retrieve encryption key
  - `cmd/debug_ble` - Live scanner with optional decryption
  - `cmd/debug_decrypt_test` - Test parsing/decryption

**Decrypted Format** (16 bytes):
```
Byte    Description                     Status      Notes
----    -----------                     ------      -----
0       Header/Flags                    ✅ Working   0x10 on Pro 3; upper nibble clear on older models
1       First Pod Battery + Charging    ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
2       Second Pod Battery + Charging   ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
3       Case Battery + Charging         ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
4       Model marker                    ✅ Working   0x1D on Pro 3, 0x2D on older models
5       Unknown                         ❓          Constant 0x7D across all Pro 3 samples
6       Unknown                         ❓          Constant 0x64 across all Pro 3 samples
7-9     Real MAC suffix                 ✅ Working   Last 3 bytes of the device's permanent MAC
10-11   Padding/Unknown                 ❓          Always 00 00 in observed samples
12-15   Rotating tail                   ❓          Changes every advertisement (counter or MIC?)
```

**Observed samples** (AirPods Pro 3, real MAC `AA:BB:CC:DD:EE:FF`, two ads minutes apart):
```
10 be be 9f 1d 7d 64 [DD EE FF] 00 00 a7 8a b5 cc   -> 62% / 62% / 31%
10 c8 ca ba 1d 7d 64 [DD EE FF] 00 00 7e 5b ee 7a   -> 72% / 74% / 58%
   ^^ ^^ ^^              ^^^^^^ MAC suffix, stable
   batteries
```
Only bytes 1-3 (batteries) and 12-15 (rotating tail) change between advertisements.
Bytes 0 and 4-11 were byte-identical across every sample captured.

**Decryption Validation:**

⚠️ **The magic-byte check does not work on AirPods Pro 3.** The historical check
(byte 0 upper nibble clear, byte 4 == `0x2D`) comes from reverse engineering of
older models. Pro 3 reports byte 0 = `0x10` and byte 4 = `0x1D`, so a *correct*
decryption fails that test and gets discarded. This silently disabled 1% battery
accuracy for Pro 3 entirely.

Validate against the **MAC suffix** instead:

- Bytes 7-9 of the decrypted payload hold the last 3 bytes of the device's real MAC
- Compare them against the MAC the candidate key is stored under
- Three exact bytes make a false positive roughly 1 in 16.7 million per key tried

This is both more reliable and more useful than a magic byte: the payload
identifies *its own device*, which is exactly what is needed to resolve a
randomized BLE MAC back to a real one.

Keep the legacy magic-byte check as a fallback for pre-Pro-3 models, which have
not been re-tested against the MAC-suffix rule.

**Orientation Handling:**
- If NOT flipped (left pod primary): Byte 1=left, Byte 2=right
- If flipped (right pod primary): Byte 1=right, Byte 2=left

**Battery Validation:**
- Values > 100 indicate unavailable/unknown battery

**Important:** The encrypted portion is always the **last 16 bytes** of the payload, not a fixed byte offset. Extract using `payload[len(payload)-16:]`.

Note that every payload observed so far is exactly 25 bytes, where the last 16
bytes and the fixed range `payload[9..25]` are the same slice. Implementations
using the fixed offset therefore work today, but would break on any future
payload of a different length.

## Accuracy Limitations

### Two-Tier Battery System

**Unencrypted Data** (bytes 4-5):
- **Granularity:** 10% increments (0%, 10%, 20%, ..., 100%)
- **Accuracy:** ~10% off actual values
- **Encoding:** Nibble-based (0x0-0x9 = 0-90%, 0xA-0xE = 100%, 0xF = unknown)
- **No encryption key required**

**Encrypted Data** (bytes 9-24):
- **Granularity:** 1% increments (0-100%)
- **Accuracy:** 1% accuracy (matches actual battery)
- **Encoding:** Bit 7 = charging, bits 0-6 = level
- **Requires ENC_KEY from AAP connection (See [AAP Key Retrieval](aap-key-retrieval.md))**

### Update Frequency

- **BLE Advertisements:** 30-60 seconds (slow)
- **Real-time:** No - values are cached/delayed
- **Recommendation:** Use AAP for real-time battery monitoring

### Ear Detection

✅ **Working** - Reliably detects when AirPods are in/out of ears
- Encoded in byte 3 (status byte), bits 1 and 3
- Affected by orientation (uses XOR logic)

### Lid Status

❌ **TO FIX** - Byte 6 and byte 8 appear related to lid status but format is unknown/unreliable

### Connection State

❌ **TO FIX** - Byte 8 may contain connection state but parsing is currently unreliable

## Comparison: BLE vs AAP

### BLE Proximity Pairing (This Protocol)

| Feature | Unencrypted | Encrypted |
|---------|-------------|-----------|
| Battery Accuracy | ±10% | 1% |
| Battery Granularity | 10% increments | 1% increments |
| Update Rate | 30-60s | 30-60s |
| Connection Required | No | No (but needs key from AAP) |
| Works with iPhone connected | Yes | Yes |
| Control (Noise modes) | No | No |
| Encryption Key Required | No | Yes (from AAP) |

### AAP (Apple Accessory Protocol)

| Feature                             | Status |
|-------------------------------------|--------|
| Accuracy                            | 1% (real-time) |
| Update Rate                         | <1s |
| Connection Required                 | Yes (L2CAP PSM 4097) |
| Works with iPhone connected         | No (disconnects iPhone) |
| Control Commands (e.g. Noise modes) | Yes |
| Battery granularity                 | 1% increments |

**Recommendation:**
- Use **AAP** for real-time battery and control when AirPods connected to Linux
- Use **BLE (unencrypted)** for quick approximate monitoring when connected to other devices
- Use **BLE (encrypted)** for accurate passive monitoring (requires one-time key retrieval via AAP)

## MAC Address Randomization and Device Identification

### BLE Privacy Feature

AirPods use **Bluetooth LE Privacy** (MAC address randomization) in their BLE advertisements:

- **BLE Advertisements**: Use randomized MAC addresses that change periodically
- **AAP Connections**: Use the real (permanent) MAC address
- **Privacy Goal**: Prevent tracking of AirPods via BLE advertisements

### Device Identification Challenge

When monitoring multiple AirPods devices:
1. Each device has a real MAC address (from AAP connection)
2. Each device's BLE advertisements use a different randomized MAC
3. The randomized MAC changes periodically (every few minutes to hours)
4. **Problem**: Cannot directly match BLE advertisements to stored encryption keys

### Solution: Decryption-Based Identification

To identify which device a BLE advertisement belongs to:

1. **Store encryption keys by real MAC**: When retrieving keys via AAP, store them with the AAP connection's MAC address
2. **Try all keys on BLE data**: When receiving a BLE advertisement, attempt decryption with all stored keys
3. **Validate decryption**: Check if decrypted data matches expected format
4. **Identify device**: The key that successfully decrypts identifies the real device

### Decryption Validation

Wrong keys produce garbage, but AES always "succeeds", so the decrypted payload
must be checked before it is trusted.

**Preferred check - MAC suffix (works on Pro 3 and is self-identifying):**

Bytes 7-9 of the decrypted payload carry the last 3 bytes of the device's real
MAC. Compare them against the MAC the candidate key is stored under:

```rust
fn matches_device(decrypted: &[u8; 16], mac_addr: &str) -> bool {
    let Some(suffix) = mac_suffix(mac_addr) else { return false };
    decrypted[7..10] == suffix
}
```

**Fallback - legacy magic bytes (pre-Pro-3 models only):**

```rust
fn has_legacy_magic(decrypted: &[u8; 16]) -> bool {
    (decrypted[0] & 0xF0) == 0 && decrypted[4] == 0x2D
}
```

Accept a decryption if **either** check passes. Do not rely on the magic bytes
alone - see the warning under [Bytes 9-24](#bytes-9-24-encrypted-battery-data).

### Multi-Device Workflow

```
Device A (Real MAC: 11:22:33:44:55:66)
  └─> Store encryption key: 11:22:33:44:55:66 -> [16-byte key]

Device B (Real MAC: AA:BB:CC:DD:EE:FF)
  └─> Store encryption key: AA:BB:CC:DD:EE:FF -> [16-byte key]

BLE Advertisement received (Random MAC: 77:88:99:00:11:22)
  ├─> Try decrypt with key from 11:22:33:44:55:66
  │     └─> decrypted[7..10] = DD EE FF != 44 55 66 -> ❌ not this device
  └─> Try decrypt with key from AA:BB:CC:DD:EE:FF
        └─> decrypted[7..10] = DD EE FF == DD EE FF -> ✅ match
            └─> Device identified as AA:BB:CC:DD:EE:FF
```

Because identification resolves the randomized MAC back to the permanent one,
per-device state should be keyed by the **real** MAC. Keying by the advertised
MAC instead makes state grow without bound, since Apple rotates it continuously
while disconnected (observed: 4 distinct MACs for one device inside 45 seconds).
Devices that cannot be identified - no stored key, or a key that does not match -
still need a time-based eviction policy, since their rotating MACs cannot be
collapsed.

## Implementation Notes

### Error Handling

- Advertisement packets may be intermittent
- Check payload length before accessing bytes
- Handle missing/incomplete packets gracefully
- Battery values > 100 indicate unavailable (parser returns nil pointer)
- Parser automatically handles orientation (IsFlipped)

## References

- [LibrePods](https://github.com/kavishdevar/librepods) - Open source Android and Linux AirPods client
- [OpenPods](https://github.com/adolfintel/OpenPods) - Open source Android AirPods client
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity protocol documentation
- BlueZ D-Bus API documentation

## Known Limitations

1. **Update latency** - BLE advertisements update every 30-60 seconds (inherent to protocol)
2. **Unencrypted battery accuracy** - ~10% granularity, may be off by up to 10% (use encrypted data for 1% accuracy)
3. **Encryption key requirement** - Accurate (1%) battery requires one-time key retrieval via AAP connection
4. **Decrypted layout is model-dependent** - Byte 0 and byte 4 differ between Pro 3
   and older models. Validate by MAC suffix (bytes 7-9) rather than by magic bytes;
   see [Decryption Validation](#decryption-validation)
5. **Bytes 5-6 and 12-15 unidentified** - Bytes 5 (`0x7D`) and 6 (`0x64`) are constant
   across all captured Pro 3 samples; bytes 12-15 change on every advertisement and are
   presumed to be a counter or MIC. None have been confirmed
6. **MAC-suffix rule unverified on older models** - Confirmed on AirPods Pro 3 (0x2720)
   only. Pre-Pro-3 devices still rely on the legacy magic-byte fallback

---

**Last Updated:** 2026-09-09<br>
**Tested With:**
 - AirPods Pro (Gen 2) (0x2420), Firmware 7A305
 - AirPods Pro 3 (0x2720), Firmware 8A353

**Confidence note:** The Pro 3 decrypted layout documented here was derived from a
handful of advertisements captured from a single device on 2026-09-09. Byte
positions 1-3 (batteries) and 7-9 (MAC suffix) are confirmed - the batteries track
the cleartext values and the suffix matches the device's real MAC across every
sample. The remaining fields are inference from constancy, not verified meaning.

