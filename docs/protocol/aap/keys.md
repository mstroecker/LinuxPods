# AAP keys

The key request, the key packet that answers it, and the two keys it carries: the IRK and
the ENC_KEY. The request and the layout come from LibrePods; the packet was captured on
AirPods Pro 3 (#71) and both keys used on a Pro 3 and a Pro 2 (USB-C) (#134), where the
IRK's byte order was found.

Read by `aap::keys`; what the app does with the keys: [app/keystore.md](../../app/keystore.md)
and [app/identification.md](../../app/identification.md).

Byte offsets count from 0 at the first byte of the packet.

## Why This Matters

AirPods advertise over BLE from addresses that rotate, and encrypt the last 16 bytes of
the advertisement. The two keys undo both:

| Key | What it opens |
|---|---|
| IRK | The advertising address, a resolvable private address: it tells which pair sent the advertisement ([protocol/ble/addresses.md](../ble/addresses.md#resolving-the-advertising-address)). |
| ENC_KEY | The encrypted part: the last three bytes of the pair's real MAC, and the exact battery levels ([protocol/ble/proximity-pairing.md](../ble/proximity-pairing.md#decrypting-ble-advertisements)). |

With them, a client reads a pair's advertisements while the pair is disconnected or
connected to another device. AAP gives the exact levels too, but only over a connection.

What LinuxPods counts without the keys: [app/identification.md](../../app/identification.md#what-counts).

## Key Retrieval Protocol

Steps 1 and 2, the connection and the handshake: [session.md](session.md#step-1-connect-to-airpods).
The key request is one of the requests sent after the handshake's answer
([session.md](session.md#step-3-send-the-requests)).

The request and the packet's layout come from [LibrePods](https://github.com/librepods-org/librepods)'
implementation; its AAP Definitions do not describe them.

### Step 3: Send Key Request

```
04 00 04 00 30 00 05 00
```

8 bytes, opcode `0x30`.

### Step 4: Wait for Key Response

The pods answer with one key packet, among the other packets of the link-up: a Pro 3
sent it 350 ms and 385 ms after the request, between the first and the second `0x0C`
(#71 captures 1 and 2; [session.md](session.md#initial-state-report)). Not every packet
that comes is the key packet.

#### Identifying Key Packets

A packet is the key packet if it has the header `04 00 04 00`, `31` at byte 4, and at
least 7 bytes.

The header comes first: byte 4 is an opcode only after it
([session.md](session.md#packets)).

#### Key Packet Format

| Bytes | Field | Value |
|---|---|---|
| 0-3 | Header | `04 00 04 00` |
| 4 | Opcode | `31` |
| 5 | Unknown | `00` |
| 6 | Key count | `02` |
| 7 on | Keys | One after the other, each a 4-byte key header and the key |

Each key:

| Bytes, from the key's start | Field | Value |
|---|---|---|
| +0 | Key type | `01` IRK, `04` ENC_KEY |
| +1 | Unknown | `00` |
| +2 | Key length, in bytes | `10` (16) |
| +3 | Unknown | `00` |
| +4 on | The key | 16 bytes |

A Pro 3 sent 47 bytes: `04 00 04 00 31 00 02`, then the IRK and the ENC_KEY, each with a
`[type] 00 10 00` header and 16 bytes (#71 capture 1). The two came in that order.

### Step 5: Parse Keys

Read the count at byte 6, then, from byte 7, each key: its type at +0, its length at +2,
and that many bytes from +4; the next key starts after them.
[`examples/key_request.rs`](../../../examples/key_request.rs) does it, and prints each
key's type and length, never its bytes.

## Key Types

### IRK (Identity Resolving Key)

- **Type code:** `01`.
- **Length:** 16 bytes.
- **Purpose:** resolves the resolvable private addresses the pair advertises from, so
  it tells which pair an advertisement comes from. It does not decrypt the
  advertisement: the ENC_KEY does.
- **Byte order: reversed.** The packet carries the IRK least significant byte first, as
  SMP carries keys. The address hash `ah()` (Core spec Vol 3 Part H 2.2.2) takes it most
  significant byte first, so it has to be reversed before use; as it comes, it resolves
  nothing. Found on AirPods Pro 3 and Pro 2 (USB-C) with
  [`examples/irk_probe.rs`](../../../examples/irk_probe.rs) (2026-10-28, build fb3c4a6,
  #134).

### ENC_KEY (Encryption Key)

- **Type code:** `04`.
- **Length:** 16 bytes.
- **Purpose:** decrypts the encrypted part of the pair's proximity pairing
  advertisements.

## Example Response

A key packet with synthetic keys:

| Bytes | Field |
|---|---|
| `04 00 04 00` | Header |
| `31` | Opcode |
| `00` | Unknown |
| `02` | Key count: 2 |
| `01 00 10 00` | IRK, 16 bytes |
| `aa` × 16 | The IRK |
| `04 00 10 00` | ENC_KEY, 16 bytes |
| `bb` × 16 | The ENC_KEY |

## Using the Keys

- Resolving the advertising address with the IRK: [protocol/ble/addresses.md](../ble/addresses.md#resolving-the-advertising-address)
- Decrypting the advertisement with the ENC_KEY: [protocol/ble/proximity-pairing.md](../ble/proximity-pairing.md#decrypting-ble-advertisements)

## Key Persistence

The keys are the pair's, and stay the same across connections, so they can be kept and
used again:

- **Every connection:** the same keys came back (Pro 3, #71 capture 2: the same key as in
  September 2026, so the pair had not been reset in between).
- **Re-pairing:** an AirPods Pro 2 (USB-C), forgotten in GNOME Settings and paired again
  with the same computer, sent the same ENC_KEY and IRK as before (2026-11-01, f9620ca).

Why LinuxPods requests them on every connection anyway, and where it keeps them: [app/keystore.md](../../app/keystore.md#requested-on-every-connection).

## Supported Devices

The key packet came from AirPods Pro 3 (#71) and AirPods Pro 2 (USB-C) (#134). On
2026-11-02 both pairs ran firmware `9A348` ([device-info.md](device-info.md#the-firmware-version-field-10)).

## References

- [LibrePods](https://github.com/librepods-org/librepods) - the key request and the
  key packet's layout
- [BLE Proximity Pairing Protocol](../ble/proximity-pairing.md) - the advertisement the
  keys open

## Open questions

- Whether any model or firmware sends no key packet.
- Whether anything changes the keys: a reset, a pairing with another Apple account, more
  time. Untested.
- Other models: AirPods Pro (1st gen), AirPods (2nd and 3rd gen), AirPods Max, and Beats
  with Apple's H1 or H2 chip were listed as likely to work. None was tried.
- What bytes 5, +1 and +3 of the key packet hold: `00` in every packet seen.
