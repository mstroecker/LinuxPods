# Apple Accessory Protocol (AAP) - Key Retrieval

This document describes how to retrieve encryption keys from AirPods using the Apple Accessory Protocol (AAP) over L2CAP. These keys are used to decrypt the encrypted portion of BLE proximity pairing advertisements.

## Overview

AirPods encrypt the last 16 bytes of their BLE proximity pairing advertisements. To decrypt this data, you need to retrieve encryption keys from the AirPods via an AAP connection.

### Why This Matters

- **BLE advertisements** come from random addresses. The cleartext part says nothing about which pair sent them
- **The encrypted part** carries the last three bytes of the pair's real MAC, and the exact batteries. A key that decrypts it identifies the pair
- **Direct AAP connection**: exact batteries too, but requires an active connection

By retrieving keys via AAP, you can identify a pair's BLE advertisements and read its batteries even when the AirPods are disconnected or connected to another device. LinuxPods drops every advertisement no stored key decrypts, so without the key a pair does not show over BLE at all.

## Connection Details

### Protocol
- **Transport**: L2CAP (Logical Link Control and Adaptation Protocol)
- **PSM**: 0x1001 (4097 decimal)
- **Connection Type**: BR/EDR (Classic Bluetooth)
- **Socket Type**: SOCK_SEQPACKET

### Prerequisites
- AirPods must be paired with the system via Bluetooth
- AirPods must be powered on and in range
- Bluetooth adapter with L2CAP support

## Key Retrieval Protocol

### Step 1: Connect to AirPods

Establish an L2CAP connection to the AirPods on PSM 0x1001:

```rust
use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType};

let sa = SocketAddr { addr, addr_type: AddressType::BrEdr, psm: 0x1001, cid: 0 };
let socket = Socket::<SeqPacket>::new_seq_packet()?;
let stream = socket.connect(sa).await?;
```

⚠️ A non-blocking connect returns almost immediately and reports success even when
the BR/EDR ACL link is not yet up. **Check `peer_addr().cid` straight after
connecting**: a zero cid means no channel was established, every send will fail with
`ENOTCONN`, and the socket cannot recover - discard it and retry with a fresh one.

### Step 2: Send Handshake

Send a 16-byte handshake packet to initialize the connection:

```
Hex: 00 00 04 00 01 00 02 00 00 00 00 00 00 00 00 00
```

**Expected response:** a packet starting `01 00 04 00` (18 bytes on Pro 3).

⚠️ **Wait for it before sending anything else.** The AirPods drop what they are sent
before they have answered. A pair that had only just connected answered 1.2 s after
the handshake, and requests sent after a fixed 500 ms pause were never answered - no
battery, no keys. `Client::handshake` returns once the answer is in, and fails if
none comes within 5 s.

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
- **Byte [4] == 0x31** (key data marker)

Example check:
```rust
if packet.len() >= 7 && packet[4] == 0x31 {
    // This packet contains keys
}
```

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
        0x01 => println!("IRK: {key_bytes:02x?}"),
        0x04 => println!("ENC_KEY: {key_bytes:02x?}"),
        _ => {}
    }
}
```

## Key Types

### IRK (Identity Resolving Key)
- **Type Code**: 0x01
- **Length**: 16 bytes (128 bits)
- **Purpose**: Resolve Bluetooth Resolvable Private Addresses
- **Use for decryption**: Generally **not used** for BLE payload decryption

### ENC_KEY (Encryption Key)
- **Type Code**: 0x04
- **Length**: 16 bytes (128 bits)
- **Purpose**: Encrypt/decrypt BLE advertisement payloads
- **Use for decryption**: **Primary key** for decrypting BLE proximity pairing data

## Example Response

```
Hex dump of key response packet:

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
64 dd 9c e7  Key Data
bc b5 b5 0a
9a 33 45 42
cb af 19 02
04           Key Type (ENC_KEY)
00           Unknown
10           Key Length (16 bytes)
00           Unknown
1b dd 73 d2  Key Data
be 01 9b 98
55 cc 5b 36
7b f9 9d 5d
```

**Parsed output:**
```
Key 1: IRK (Identity Resolving Key)
  64dd9ce7bcb5b50a9a334542cbaf1902

Key 2: ENC_KEY (Encryption Key)
  1bdd73d2be019b9855cc5b367bf99d5d
```

## Using the Keys

### Decrypting BLE Advertisements

Once you have the **ENC_KEY** (type 0x04), you can decrypt BLE proximity pairing advertisements:

1. **Extract encrypted portion**: last 16 bytes of the BLE payload
2. **Decrypt**: AES-128 ECB, single block, no padding
3. **Validate**: bytes 7-9 hold the last three bytes of the device's real MAC
4. **Parse**: bytes 1-3 are first pod, second pod and case (bit 7 = charging)

```rust
use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};

let cipher = Aes128::new_from_slice(enc_key)?;
let mut block = Block::<Aes128>::try_from(encrypted)?;
cipher.decrypt_block(&mut block);
// block now holds the 16-byte plaintext
```

AES always "succeeds", so step 3 is what tells a correct key from a wrong one - and
because the payload carries its own MAC, it also identifies which device sent it.

See [BLE Proximity Pairing documentation](ble-proximity-pairing.md) for details on the decrypted payload format.

## Implementation Notes

### Multiple Packets

The AirPods may send several packets after the key request:
- Some packets are status updates, or the unsolicited list of known hosts (`04 00 04 00 2B 00`),
  which comes with the handshake's answer. It is not an answer to the features packet, which
  nothing answers (#71)
- Only packets with `data[4] == 0x31` contain keys
- **Loop and check each packet** until keys are found

### Timeout Handling

```rust
// Bounded by time: a count of packets runs out only while packets keep coming,
// and a silent device sends none
let keys = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
        let packet = client.read_packet().await?;
        if packet.len() >= 7 && packet[4] == 0x31 {
            return Ok::<_, anyhow::Error>(parse_proximity_keys(&packet)?);
        }
    }
})
.await??;
```

### Key Persistence

Keys are device-specific and persistent:
- **Same keys** for the lifetime of the AirPods pairing
- Can be cached and reused
- Request them on every connection anyway: re-pairing, from any of the user's devices,
  regenerates them, and a stale key fails suffix validation without a word
- Store securely (keyring, encrypted config file)

### Error Handling

Common failure modes:
- **Connection timeout**: AirPods out of range or powered off
- **Connection refused**: AirPods not paired with system
- **No key packet**: AirPods firmware doesn't support key retrieval (unlikely for recent models)
- **Malformed response**: Parsing error, retry connection

## Reference Implementation

The app itself asks for the keys on every AAP connection: `Coordinator::open_aap`
sends the request once the session is up, and the link's read loop hands each key
packet to `aap::find_encryption_key`, storing a 16-byte key that differs from the one
held. A re-paired pair's new key therefore arrives with the next connection, and a
request that went unanswered is sent again by reconnecting. Run the app with the
AirPods connected; a key it had not saved yet is logged as it is written:
```
Stored encryption key for <MAC> (16 bytes)
```

To debug a request that fails - on a new model, say - the `key_request` example
opens the same session, sends the request, and lists every packet that arrives,
laying a key packet out key by key:
```bash
cargo run --example key_request -- <MAC_ADDRESS> [SECONDS]
```

It prints each packet's opcode, length and kind; for a key packet, its header, its
total length and each key's type and length, and whether the app would take its
ENC_KEY (the 16-byte rule of `find_encryption_key`). Key bytes are never printed,
so the output can go into an issue, and nothing is stored: a debugging run must not
replace a key that works. If no key packet arrives within the window, 5 s by
default, it says so and lists what did arrive.

## Testing

Use the retrieved ENC_KEY to test decryption:

```bash
# Test with a known BLE payload
cargo run --example decrypt_probe

# Live scanning with decryption
RUST_LOG=linuxpods=debug cargo run
```

If decryption is working correctly, you should see:
- Byte 1: Left/Right pod battery (0-100%)
- Byte 2: Right/Left pod battery (0-100%)
- Bit 7 of each byte indicates charging status

## Supported Devices

Tested with:
- **AirPods Pro (2nd Gen)** - Firmware 7A305 ✅
- **AirPods Pro 3** - Firmware 8A353 ✅

Likely works with:
- AirPods Pro (1st Gen)
- AirPods (2nd/3rd Gen)
- AirPods Max
- Beats with Apple H1/H2 chip

## Troubleshooting

### "Connection refused"
- Ensure AirPods are paired via Bluetooth settings
- Check AirPods are powered on and in range

### "No key packet received"
- Wait longer: the key packet may come seconds after the request
- Check PSM is correct (0x1001)
- Verify nothing is sent before the handshake is answered (`01 00 04 00 …`): a key
  request sent sooner is dropped, with no error

### "Decryption produces garbage"
- Wrong key type (use ENC_KEY, not IRK)
- Wrong key extracted (check parsing logic)
- BLE payload from a different AirPods device - expected when trying every stored
  key; the MAC suffix at bytes 7-9 is what tells you which one matched
- Validating with magic bytes rather than the MAC suffix. The old marker (byte 0
  upper nibble `0x0`, byte 4 `0x2D`) rejects correct decryptions on current models

### "Keys change after re-pairing"
- Expected behavior
- Keys are regenerated when AirPods are re-paired
- Re-retrieve keys after pairing

## References

- [LibrePods](https://github.com/kavishdevar/librepods) - Original implementation
- [BLE Proximity Pairing Protocol](ble-proximity-pairing.md) - BLE advertisement format
- Bluetooth Core Specification - L2CAP protocol details

---

- **Last Updated:** 2026-10-28
- **Tested With:** AirPods Pro (2nd Gen) 0x2024, AirPods Pro 3 0x2027
