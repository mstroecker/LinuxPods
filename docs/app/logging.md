# Logging

What LinuxPods logs of the packets and advertisements it reads, and what it never logs.
Logs and probe output get pasted into issues, so what they must never show matters as
much as what they show.

## Never keys or serial numbers

No log, probe or example prints key bytes or serial numbers.

- **Bytes go through `linuxpods::hex`:** two lowercase digits each, a space between.
- **A packet that may carry keys or serial numbers goes through
  `aap::keys::redacted_hex`,** which shows only its preamble, and a note:

| Packet | What is shown |
|---|---|
| A key packet (`0x31`) | Its first `aap::keys::PREAMBLE_LEN` bytes: no key |
| A device information packet (`0x1D`) | Its first `aap::info::PREAMBLE_LEN` bytes: no name, no serial number |

Any packet with either opcode is cut short, whether it parses or not.

## AAP packets

- `AAP packet (N bytes):` at `debug` shows a packet's first 8 bytes, redacted: of a key
  packet, its 7-byte preamble alone.
- `AAP packet (N bytes), whole:` at `trace` shows the whole packet, redacted, for what
  lies past those 8 bytes: `0x08`'s last two took a local build to see (#142).

## BLE advertisements

Each stage is logged on its own:

| Line | Level | What it covers |
|---|---|---|
| `BLE parsable:` | `debug` | Every advertisement that parses, including the ones the coordinator then drops, and the payload read of a device an advertisement monitor reports found that no signal from bluetoothd has brought since it was last lost ([bluez.md](bluez.md#listening-for-the-pods)) |
| `BLE parsable raw:` | `trace` | Its payload |
| `BLE decryptable:` | `debug` | Identification: which pair, and whether by the IRK or by the key alone ([identification.md](identification.md)) |

`BLE parsable raw:` prints the payload from its first byte after the type and length
bytes, so its offsets are those of
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#payload-structure),
as are the parser's.

Apple manufacturer data that is not proximity pairing is noisy, and goes to `trace`.

## Device information

The packet's fields, serial numbers among them:
[protocol/aap/device-info.md](../protocol/aap/device-info.md#fields).

- **Read only when its shape is right:** a `0x1D` without `02` at byte 6, or without
  `04 00` at bytes 9-10, is not read at all (`aap::info::is_info_packet`), as a `0x09`
  report is not read for a mode unless byte 6 names the setting. It is redacted all the
  same, by its opcode.
- **Only fields 0 and 10 leave the parser:** the pods' name and the firmware version. The
  serial numbers are never parsed out, logged, kept or shown.
- **An error carries none of a field's bytes,** as a field out of place could be a serial
  number.

## Log text

Log text that describes control flow goes stale like comments, and is more visible: it is
what a user pastes. A line says what happened, not what the code will do next.

## Hardware checks

None open.
