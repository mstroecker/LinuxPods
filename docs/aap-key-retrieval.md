# Apple Accessory Protocol (AAP) - Key Retrieval

This document describes how to retrieve encryption keys from AirPods using the Apple Accessory Protocol (AAP) over L2CAP. These keys are used to tell which pair a BLE proximity pairing advertisement comes from, and to decrypt its encrypted portion.

## Overview

AirPods encrypt the last 16 bytes of their BLE proximity pairing advertisements. To decrypt this data, you need to retrieve encryption keys from the AirPods via an AAP connection.

### Why This Matters

- **BLE advertisements** come from resolvable private addresses that rotate. The cleartext part says nothing about which pair sent them, but the pair's **IRK** resolves the address
- **The encrypted part** carries the last three bytes of the pair's real MAC, and the exact batteries. The pair's **ENC_KEY** decrypts it
- **Direct AAP connection**: exact batteries too, but requires an active connection

By retrieving keys via AAP, you can identify a pair's BLE advertisements and read its batteries even when the AirPods are disconnected or connected to another device. LinuxPods counts an advertisement only if its address resolves with a stored IRK and the same pair's ENC_KEY decrypts it, so without the keys a pair does not show over BLE at all.

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

⚠️ bluer's connect can report success with the channel still connecting: a zero
`peer_addr().cid` means the connect has not finished, and every send fails with
`ENOTCONN` until it has. **Wait for the channel, and never drop the socket while it is
pending**: `conn_info()` reads back once the channel is up, and `peer_addr()` fails
`ENOTCONN` once it has closed. A socket closed while connecting leaves the device
holding a channel this end has forgotten, and a retry that gets the same local channel
shares it with that orphan (#136). `Client::connect` does the waiting.

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

### Resolving the Advertising Address

The **IRK** (type 0x01) tells which pair an advertisement comes from. The address is
a resolvable private address: its top two bits are `01`, the first three bytes are
`prand` and the last three a hash, as the address is written. Reverse the IRK, then
check that the low 24 bits of `AES-128(IRK, 0^104 || prand)` equal the hash.
`ble::rpa::resolves` does that with the IRK as AAP sends it. See
[Device Identification](ble-proximity-pairing.md#solution-identification-by-irk-then-enc_key).

### Decrypting BLE Advertisements

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
because the payload carries its own MAC. LinuxPods tries only the key of the pair
whose IRK resolved the address. A pair keyed before LinuxPods kept IRKs has its key
tried on every advertisement no stored IRK resolves, until its next connection
brings its IRK.

See [BLE Proximity Pairing documentation](ble-proximity-pairing.md) for details on the decrypted payload format.

## Implementation Notes

### Multiple Packets

The AirPods may send several packets after the key request:
- Some packets are status updates, or the unsolicited list of known hosts (`04 00 04 00 2B 00`),
  which comes with the handshake's answer. It is not an answer to the features packet, which
  nothing answers (#71)
- Only packets with the `04 00 04 00` header and `data[4] == 0x31` contain keys
- **Loop and check each packet** until keys are found

### Timeout Handling

```rust
// Bounded by time: a count of packets runs out only while packets keep coming,
// and a silent device sends none
let keys = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
        let packet = client.read_packet().await?;
        if packet.len() >= 7 && packet[..4] == [0x04, 0x00, 0x04, 0x00] && packet[4] == 0x31 {
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
  regenerates them. A stale IRK resolves none of the pair's new addresses, and a stale
  key fails suffix validation, both without a word
- Store securely (keyring, encrypted config file). LinuxPods keeps them in
  `~/.local/share/linuxpods/keys.json`, mode 0600: the ENC_KEY and, since format
  version 2, the IRK, as AAP sends it

### Error Handling

Common failure modes:
- **Connection timeout**: AirPods out of range or powered off
- **Connection refused**: AirPods not paired with system
- **No key packet**: AirPods firmware doesn't support key retrieval (unlikely for recent models)
- **Malformed response**: Parsing error, retry connection

## Reference Implementation

The app itself asks for the keys on every AAP connection: `Coordinator::open_aap`
sends the request once the session is up, and the link's read loop hands each key
packet to `aap::find_encryption_key` and `aap::find_irk`, which each take a 16-byte
key. A packet without a 16-byte ENC_KEY is rejected; one without a 16-byte IRK keeps
the IRK stored, as long as the ENC_KEY is the one stored with it. The keys go into
use at once, and are written to the keystore whenever the keys in memory differ from
those last saved successfully - so a save that failed is tried again when the keys
next arrive. A re-paired pair's new keys therefore arrive with the next connection,
and a request that went unanswered is sent again by reconnecting. Run the app with
the AirPods connected; keys it had not saved yet are logged as they are written:
```
Stored the keys of <MAC>
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

To check that a new model's advertising addresses resolve with its IRK, in the
order the app uses, `irk_probe` asks each pair named for its keys, keeps them in
memory, and scans, printing per address only whether it resolves and decrypts:
```bash
cargo run --example irk_probe -- <MAC_ADDRESS> [<MAC_ADDRESS>...]
```
It prints no keys and no addresses, and stores nothing.

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
  key; resolve the address with each IRK first, and the MAC suffix at bytes 7-9
  confirms the match
- Validating with magic bytes rather than the MAC suffix. The old marker (byte 0
  upper nibble `0x0`, byte 4 `0x2D`) rejects correct decryptions on current models

### "Keys change after re-pairing"
- Expected behavior
- Keys are regenerated when AirPods are re-paired
- Re-retrieve keys after pairing: the IRK changes too, so the old one resolves none
  of the new addresses

## References

- [LibrePods](https://github.com/librepods-org/librepods) - Original implementation
- [BLE Proximity Pairing Protocol](ble-proximity-pairing.md) - BLE advertisement format
- Bluetooth Core Specification - L2CAP protocol details

---

- **Last Updated:** 2026-10-28
- **Tested With:** AirPods Pro (2nd Gen) 0x2024, AirPods Pro 3 0x2027
