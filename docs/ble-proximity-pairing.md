# Apple Continuity BLE Proximity Pairing Protocol

This document describes the reverse-engineered Apple Continuity Proximity Pairing protocol used by AirPods to broadcast battery and status information via Bluetooth Low Energy (BLE) advertisements.

## Overview

AirPods continuously broadcast BLE advertisements containing battery levels, charging status, and device information. This allows nearby devices to display battery information **without establishing an active connection**.

### Key Characteristics

- **Passive Monitoring**: No connection required
- **Approximate Data**: Battery levels may be 5–10% off actual values (Encrypted data not yet supported)
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
Byte    Description                     Example     Notes
----    -----------                     -------     -----
0       Prefix                          0x01        Always 0x01
1-2     Device Model (Big-Endian)       0x2420      0x2420 = AirPods Pro
3       Status Byte                     0x0b        See Status Byte section (ear detection)
4       Battery Levels                  0x88        See Battery Levels section
5       Charging Status + Case Battery  0x07        See Charging Status section
6       Lid Open Counter                0x08        See Case Battery section
7       Device Color                    0x00        Not tested
8       Unknown, maybe encrypted        0x05        Not clear
9-24    Encrypted Data                  ...         Encrypted payload (16 bytes)
```

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

Todo

### Byte 7: Device Color

not tested

### Byte 8: Unknown (Lid Status?)

Todo


### Bytes 8-24: Encrypted Data

The final 16 bytes (last 16 bytes of the payload) are encrypted and contain additional device-specific information. The encryption key (IRK or ENC_KEY) is obtained via the AAP protocol.

**Important:** The encrypted portion is always the **last 16 bytes** of the payload, not a fixed byte offset. The total payload length may vary between AirPods models and firmware versions.

**Decryption:**
- Algorithm: AES-128 ECB mode (no padding, no IV)
- Key source: Retrieved via AAP connection (PSM 4097)
- Key types: IRK (Identity Resolving Key) or ENC_KEY
- See `cmd/debug_proximity_keys` for key retrieval
- See `cmd/debug_ble_decrypt` for decryption example

## Accuracy Limitations

### Battery Levels (Left/Right AirPods)

- **Advertised:** Rounded to nearest 10%
- **Actual:** May differ by 1-10%
- **Update Frequency:** 30-60 seconds (slow)
- **Real-time:** No - values are cached/delayed

**Example:**
```
BLE Advertisement:  80%
Actual Battery:     89%
Difference:         9% off
```

### Case Battery

- **Advertised:** Approximate value (nibble-encoded like AirPods)
- **Actual:** May differ by 1-10%
- **Accuracy:** Similar to AirPods battery

### Ear Detection

- **Reliability:** Dependent on device model and firmware
- **Calibration:** May require device-specific calibration
- **Use Cases:** Detecting when AirPods are in/out of ears

### Lid Status (Encrypted?)

ToDo

- **Reliability:** Variable across different AirPods models
- **Detection:** Uses byte 8, bit 3
- **Recommendation:** Test thoroughly with your specific model

## Comparison: BLE vs AAP

### BLE Proximity Pairing (This Protocol)

| Feature | Status |
|---------|--------|
| Accuracy | ±1-10% |
| Update Rate | 30-60s |
| Connection Required | No |
| Works with iPhone connected | Yes |
| Control (Noise modes) | No |
| Battery granularity | 10% increments |

### AAP (Apple Accessory Protocol)

| Feature | Status |
|---------|--------|
| Accuracy | Real-time, accurate |
| Update Rate | <1s |
| Connection Required | Yes (L2CAP PSM 4097) |
| Works with iPhone connected | No (disconnects iPhone) |
| Control (Noise modes) | Yes |
| Battery granularity | 1% increments |

**Recommendation:** Use AAP for accurate battery readings and control. Use BLE for passive monitoring when AirPods are connected to another device.

## Implementation Notes

### Parsing ManufacturerData

```go
// Listen for PropertiesChanged signals
changes := signal.Body[1].(map[string]dbus.Variant)
mfgData := changes["ManufacturerData"].Value().(map[uint16]dbus.Variant)

// Look for Apple manufacturer data (0x004C)
if appleData, ok := mfgData[0x004C]; ok {
    data := appleData.Value().([]byte)

    // Check for proximity pairing (type 0x07)
    if data[0] == 0x07 {
        length := int(data[1])
        payload := data[2 : 2+length]

        // Parse payload...
    }
}
```

### Error Handling

- Advertisement packets may be intermittent
- Check payload length before accessing bytes
- Handle missing/incomplete packets gracefully
- Clamp battery values to 0-100% range

## Privacy Considerations

BLE proximity pairing advertisements can be used to:
- Track specific AirPods devices (unique encrypted data)
- Estimate battery levels without pairing
- Identify device model
- Detect when AirPods are in/out of ears
- Detect when AirPods are in/out of case

These advertisements are broadcast continuously and can be received by any nearby device with a BLE scanner.

## References

- [LibrePods](https://github.com/kavishdevar/librepods) - Open source Android and Linux AirPods client
- [OpenPods](https://github.com/adolfintel/OpenPods) - Open source Android AirPods client
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity protocol documentation
- BlueZ D-Bus API documentation

## Testing Data

This documentation is based on extensive testing with **AirPods Pro (Model 0x2420)** running firmware 7A305. Other models may use slightly different encodings.

### Test Scenarios Validated

- ✅ Both AirPods in case
- ✅ One AirPod in case charging
- ✅ Both AirPods out of case
- ✅ Case charging via USB-C
- ✅ Various battery levels (0-100%)

### Known Issues

1. **Battery accuracy** - 5-10% discrepancy is normal for BLE advertisements
2. **Update latency** - May take 30-60 seconds to reflect changes
3. **Ear detection calibration** - May require device-specific calibration
4. **Lid status reliability** - May not update consistently on all models

---

- **Last Updated:** 2025-10-13
- **Protocol Version:** Proximity Pairing v1 (Type 0x07)
- **Tested With:**
  - AirPods Pro (Gen 2) (0x2420), Firmware 7A305
  - AirPods Pro 3 (0x2720), Firmware 8A353
- **Based On:** [LibrePods](https://github.com/kavishdevar/librepods) BLEManager implementation
