# AAP keys

The key request, the key packet, and the IRK and ENC_KEY it carries.

This document describes how to retrieve encryption keys from AirPods using the Apple Accessory Protocol (AAP) over L2CAP. These keys are used to tell which pair a BLE proximity pairing advertisement comes from, and to decrypt its encrypted portion.

## Overview

AirPods encrypt the last 16 bytes of their BLE proximity pairing advertisements. To decrypt this data, you need to retrieve encryption keys from the AirPods via an AAP connection.

### Why This Matters

- **BLE advertisements** come from resolvable private addresses that rotate. The cleartext part says nothing about which pair sent them, but the pair's **IRK** resolves the address
- **The encrypted part** carries the last three bytes of the pair's real MAC, and the exact batteries. The pair's **ENC_KEY** decrypts it
- **Direct AAP connection**: exact batteries too, but requires an active connection

By retrieving keys via AAP, you can identify a pair's BLE advertisements and read its batteries even when the AirPods are disconnected or connected to another device.

What LinuxPods counts without the keys: [app/identification.md](../../app/identification.md#overview).

## Key Retrieval Protocol

Steps 1 and 2, the connection and the handshake: [session.md](session.md#step-1-connect-to-airpods).

### Step 3: Send Key Request

Send an 8-byte key request packet:

```
Hex: 04 00 04 00 30 00 05 00
```

### Step 4: Wait for Key Response

The AirPods will send one or more response packets. **Not all packets contain keys** - you must check for the key data marker.

#### Identifying Key Packets

A packet contains key data if:
- Length ≥ 7 bytes
- **Bytes [0..4] == `04 00 04 00`**, the header every packet after the handshake carries
- **Byte [4] == 0x31** (key data marker)

Example check:
```rust
if packet.len() >= 7 && packet[..4] == [0x04, 0x00, 0x04, 0x00] && packet[4] == 0x31 {
    // This packet contains keys
}
```

The header matters: a packet of another kind with `0x31` at byte 4 is not a key
packet. `aap::decode` checks it once for every kind of packet.

#### Key Packet Format

```
Offset   Description                 Example
------   -----------                 -------
0-1      Header                      04 00
2-3      Command type                04 00
4        Key data marker             31
5        Unknown                     00
6        Key count                   02 (2 keys)

For each key:
  +0     Key type                    01 (IRK) or 04 (ENC_KEY)
  +1     Unknown                     00
  +2     Key length (bytes)          10 (16 bytes)
  +3     Unknown                     00
  +4-N   Key data (N bytes)          [16 bytes of key data]
```

### Step 5: Parse Keys

Parse each key in the response:

```rust
let key_count = data[6] as usize;
let mut offset = 7;

for _ in 0..key_count {
    let key_type = data[offset];                 // Byte 0: type
    let key_length = data[offset + 2] as usize;  // Byte 2: length
    // bytes 1 and 3 of the header are unknown

    offset += 4; // skip the 4-byte header

    let key_bytes = &data[offset..offset + key_length];
    offset += key_length;

    // Process key...
    match key_type {
        // The length, never the bytes: printed output ends up in issues
        0x01 => println!("IRK: {} bytes", key_bytes.len()),
        0x04 => println!("ENC_KEY: {} bytes", key_bytes.len()),
        _ => {}
    }
}
```

## Key Types

### IRK (Identity Resolving Key)
- **Type Code**: 0x01
- **Length**: 16 bytes (128 bits)
- **Purpose**: Resolve the Resolvable Private Addresses the pair advertises from:
  which pair an advertisement comes from
- **Byte order**: **reversed** - least significant byte first, as SMP carries keys.
  The address hash `ah()` (Core spec Vol 3 Part H 2.2.2) takes the IRK most
  significant byte first, so it has to be reversed before use; as it comes, it
  resolves nothing. Confirmed on AirPods Pro 3 and Pro (2nd gen, USB-C) with
  `examples/irk_probe.rs` (#134)
- **Use for decryption**: **not used** for BLE payload decryption - the ENC_KEY is

### ENC_KEY (Encryption Key)
- **Type Code**: 0x04
- **Length**: 16 bytes (128 bits)
- **Purpose**: Encrypt/decrypt BLE advertisement payloads
- **Use for decryption**: **Primary key** for decrypting BLE proximity pairing data

## Example Response

```
Hex dump of key response packet, with synthetic keys:

Data         Description
------       -----------
04 00 04 00  Header
31           Key data marker
00           Unknown
02           Key Count (2 keys)
01           Key Type (IRK)
00           Unknown
10           Key Length (16 bytes)
00           Unknown
aa aa aa aa  Key Data
aa aa aa aa
aa aa aa aa
aa aa aa aa
04           Key Type (ENC_KEY)
00           Unknown
10           Key Length (16 bytes)
00           Unknown
bb bb bb bb  Key Data
bb bb bb bb
bb bb bb bb
bb bb bb bb
```

**Parsed output:**
```
Key 1: IRK (Identity Resolving Key)
  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

Key 2: ENC_KEY (Encryption Key)
  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
```

## Using the Keys

- Resolving the advertising address with the IRK: [protocol/ble/addresses.md](../ble/addresses.md#resolving-the-advertising-address)
- Decrypting the advertisement with the ENC_KEY: [protocol/ble/proximity-pairing.md](../ble/proximity-pairing.md#decrypting-ble-advertisements)

## Key Persistence

Keys are device-specific and persistent:
- The same keys come back on every connection. Re-pairing does not necessarily change
  them: on 2026-11-01, an AirPods Pro 2 (USB-C) forgotten in GNOME Settings and paired
  again with the same computer sent the same ENC_KEY and IRK as before. Whether
  anything changes them - a reset, a pairing with another Apple account, more time -
  is untested
- Can be cached and reused

Why LinuxPods requests them on every connection anyway, and where it keeps them: [app/keystore.md](../../app/keystore.md#key-persistence).

## Error Handling

Common failure modes:
- **Connection timeout**: AirPods out of range or powered off
- **Connection refused**: AirPods not paired with system
- **No key packet**: AirPods firmware doesn't support key retrieval (unlikely for recent models)
- **Malformed response**: Parsing error, retry connection

## Supported Devices

Tested with:
- **AirPods Pro (2nd Gen)** - Firmware 7A305 ✅
- **AirPods Pro 3** - Firmware 8A353 ✅

Likely works with:
- AirPods Pro (1st Gen)
- AirPods (2nd/3rd Gen)
- AirPods Max
- Beats with Apple H1/H2 chip

## References

- [LibrePods](https://github.com/librepods-org/librepods) - Original implementation
- [BLE Proximity Pairing Protocol](../ble/proximity-pairing.md) - BLE advertisement format
- Bluetooth Core Specification - L2CAP protocol details
