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

**Working Features** (unencrypted): All batteries (~10%), In Ear detection, Orientation (IsFlipped), Model, Color
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
0       Unknown                         ❓          Purpose unclear
1       First Pod Battery + Charging    ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
2       Second Pod Battery + Charging   ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
3       Case Battery + Charging         ✅ Working   Bit 7=charging, bits 0-6=level (1% accuracy)
4-15    Unknown                         ❓          Purpose unclear
```

**Orientation Handling:**
- If NOT flipped (left pod primary): Byte 1=left, Byte 2=right
- If flipped (right pod primary): Byte 1=right, Byte 2=left

**Battery Validation:**
- Values > 100 indicate unavailable/unknown battery

**Important:** The encrypted portion is always the **last 16 bytes** of the payload, not a fixed byte offset. Extract using `payload[len(payload)-16:]`.

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

---

**Last Updated:** 2025-10-16<br>
**Tested With:**
 - AirPods Pro (Gen 2) (0x2420), Firmware 7A305
 - AirPods Pro 3 (0x2720), Firmware 8A353

