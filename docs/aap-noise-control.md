# AAP Noise Control Protocol

## Overview

AirPods noise control modes (Off, ANC, Transparency, Adaptive) can be switched via the Apple Accessory Protocol (AAP) over L2CAP PSM 4097. This document describes the verified packet format, response behavior, and integration notes.

**Status:** Verified against AirPods 4 with ANC (firmware A3055) on 2026-02-15, against AirPods Pro 3 and AirPods Pro Gen 2 on 2026-09-12, and against AirPods Pro 3 in the #71 captures. `cargo run --example noise_probe -- <MAC>` reproduces the exchange below.

**Source:** Packet format originally from [LibrePods](https://github.com/kavishdevar/librepods/blob/main/AAP%20Definitions.md) reverse engineering, validated independently.

## Command Packet

```
Offset:  0  1  2  3  4  5  6  7  8  9  10
Hex:     04 00 04 00 09 00 0D MM 00 00 00
         ~~~~~~~~~~  ~~       ~~
         Header      Cmd      Mode
```

| Field | Bytes | Value | Description |
|-------|-------|-------|-------------|
| Header | 0-3 | `04 00 04 00` | Standard AAP command header |
| Command | 4 | `0x09` | Settings command family |
| Separator | 5 | `0x00` | — |
| Sub-command | 6 | `0x0D` | Noise control mode |
| Mode | 7 | See table | Target noise control mode |
| Padding | 8-10 | `00 00 00` | Reserved/unused |

**Total size:** 11 bytes

### Mode Values

| Byte | Mode | Description |
|------|------|-------------|
| `0x01` | Off | Noise control disabled |
| `0x02` | Noise Cancellation | Active Noise Cancellation (ANC) |
| `0x03` | Transparency | Pass-through audio from microphones |
| `0x04` | Adaptive | Automatically adjusts to environment |

### Full Packet Examples

```
Off:          04 00 04 00 09 00 0D 01 00 00 00
ANC:          04 00 04 00 09 00 0D 02 00 00 00
Transparency: 04 00 04 00 09 00 0D 03 00 00 00
Adaptive:     04 00 04 00 09 00 0D 04 00 00 00
```

## Prerequisites

The `EnableSpecialFeatures` packet must be sent before noise control commands. This is already part of the standard AAP connection sequence:

```
Feature Enable: 04 00 04 00 4D 00 FF 00 00 00 00 00 00 00
```

Without this packet, Adaptive mode may not be available.

### Off has a prerequisite of its own

Recent firmware treats Off as opt-in. A bare `0x0D 01` is refused: the AirPods
play an error chime and stay in the mode they were in. Confirmed on AirPods Pro 3
(0x2027) and AirPods Pro Gen 2 (0x2024), where the other three modes work
unconditionally.

Sub-command `0x34`, "Allow Off Option for Listening Mode", is the gate:

```
Allow Off:  04 00 04 00 09 00 34 01 00 00 00
Forbid Off: 04 00 04 00 09 00 34 02 00 00 00
```

Note that disabled is `0x02`, not `0x00` - the convention for the boolean
settings in this family.

Apple gates Off because loud sound reduction does not apply while noise control
is off, and the setting is the same switch the user sees on an iPhone: it
persists on the device and syncs back to their other Apple devices. So
`Coordinator::set_noise_control`, which carries out a `Command::NoiseControl`,
sends it only when Off is the mode being asked for, immediately before the mode
command, rather than enabling it on every connection. The two go out back to
back with no delay, which is enough; and as a link carries out its commands one
at a time, no other command goes out between them.

Identifier and values from [LibrePods' control command
table](https://github.com/kavishdevar/librepods/blob/main/docs/control_commands.md).

## Response Behavior

### Command Acknowledgment

What answers a mode change depends on the model, and nothing answers it reliably on all of them:

- **AirPods 4** (the test results below): every mode change produced a `0x4B` settings-changed notification, which names neither the sub-command nor the mode. A `0x0D` echo of the new mode came for perhaps one change in four.
- **AirPods Pro 3** (#71): a `0x0D` echo of every change, about 1 s after it, and no `0x4B` at all.

| Mode | Sent | `0x0D` echo | `0x4B` |
|---|---|---|---|
| Adaptive | `0D 04` | +1.1 s | none |
| Noise Cancelling | `0D 02` | +1.0 s | none |
| Off | `34 01` then `0D 01`, same ms | stale `0D 02` at +0.33 s, then `0D 01` at +1.4 s | none |
| Transparency | `0D 03` | +1.0 s | none |

**Integration note:** Treat noise control commands as fire-and-forget. Update local state optimistically after sending — do not wait for a `0x0D` confirmation packet. `Coordinator::set_noise_control` does exactly that: it records the mode as soon as the packet is away and broadcasts, and the command is done - its caller, waiting on `Coordinator::command`, hears so then. A later `0x0D` report merely confirms the mode - unless it is a stale one, below.

**Stale reports.** A `0x0D` report can carry the mode a change replaced. Switched from Noise Cancelling to Off, a pair reported `0D 02` 0.33 s later and `0D 01` only at 1.4 s (#71); taken as it came, the first put Noise Cancelling back on show for a second (#113). So a report of the replaced mode is held off until the mode set is reported, or 2 s pass - `aap::noise::ModeState`. After that any report wins again, since the mode may have been changed on another device.

A report held off is dropped, not deferred to the end of the 2 s. In run 1 below, Transparency and Adaptive each took effect and were followed by a report of the old mode, `0x02`, with no echo of the new one after it: applied at expiry, that report would show the wrong mode just the same. If a dropped report was right after all, the next report corrects the mode.

### 0x4B Settings-Changed Notification

After every noise control command, AirPods 4 respond with an identical `0x4B` packet. AirPods Pro 3 sent none:

```
Offset:  0  1  2  3  4  5  6  7  8  9
Hex:     04 00 04 00 4B 00 02 00 01 09
         ~~~~~~~~~~  ~~    ~~    ~~ ~~
         Header      Cmd   Len   Cnt Family
```

| Field | Byte | Value | Description |
|-------|------|-------|-------------|
| Header | 0-3 | `04 00 04 00` | Standard AAP header |
| Command | 4 | `0x4B` | Settings-changed notification |
| Separator | 5 | `0x00` | — |
| Data length | 6 | `0x02` | 2 bytes of payload follow (bytes 8-9) |
| Padding | 7 | `0x00` | — |
| Count | 8 | `0x01` | 1 setting changed |
| Family | 9 | `0x09` | Settings command family (0x09) that was modified |

**Total size:** 10 bytes

**Key properties:**
- The packet is **identical** regardless of which mode was set (Off, ANC, Transparency, Adaptive)
- It does **not** contain the sub-command (0x0D) or the mode value — only that *something* in the 0x09 family changed
- On AirPods 4 it is the **only consistent response** to a noise control command
- Variant `...01 08` was also observed once (run 1, ANC test) — byte 9 = `0x08` instead of `0x09`, suggesting other setting families also use this notification format
- Useful as a lightweight ACK ("command was processed") but not for determining which mode was set

### Initial State Report

On AAP connection, the AirPods send a burst of `0x09` settings packets during the startup dump. One of these is the current noise control mode:

```
04 00 04 00 09 00 0D 04 00 00 00
                     ^^ Current mode (Adaptive in this example)
```

This startup report is reliable and should be parsed to initialize the local noise control state.

On AirPods Pro 3 the dump does not come with the battery: it waits until a `0x0C` packet names this host as the one in use, which came 1.5-1.7 s after the notification request in the #71 captures. Until then the mode is unknown.

### Ear Detection Reports

AirPods Pro 3 report every change of a pod's place, unasked: `04 00 04 00 06 00`, then a byte per pod - `00` in an ear, `01` out, `02` in the case. A `0x0D` report of the unchanged mode usually follows. With exactly one pod in an ear, the pods briefly report `0D 01` (Off) and go back to the mode about 50 ms later. Neither is a mode change of the user's.

### Unsolicited Mode Changes

When the noise control mode is changed from another device (e.g., iPhone, Apple Watch), the AirPods send an unsolicited `0x0D` packet with the new mode. Parse these to keep local state synchronized - except a report of the mode a command here replaced, within 2 s of it: see Stale reports.

## Settings Command Family (0x09)

The `0x09` command byte is a family of settings sub-commands, not exclusive to noise control. Observed sub-commands during connection startup, with the data where it was recorded - the earlier runs', and AirPods Pro 3's from #71, whose dump held 21 settings:

| Sub-cmd (byte 6) | Notes | Pro 3 (#71) |
|-------------------|-------|-------------|
| `0x0D` | **Noise control mode** | the mode |
| `0x17` | Unknown | seen |
| `0x1A` | Listening mode configs - bitmask of the modes in the press-and-hold cycle | not in the dump |
| `0x34` | **Allow Off as a listening mode** - the prerequisite for `0x0D 01` | not in the dump |
| `0x18` | Unknown | seen |
| `0x1B` | Unknown | seen |
| `0x1F` | Unknown (data: `50 50`) | `50 50` |
| `0x23` | Unknown | seen |
| `0x24` | Unknown (data: `03`) | `00 03` |
| `0x25` | Unknown (data: `01`) | `01` |
| `0x26` | Unknown (data: `01`) | `02` |
| `0x28` | Unknown (data: `01`) | `02` |
| `0x29` | Unknown | seen |
| `0x2C` | Unknown | seen |
| `0x2E` | Unknown (data: `32`) | `32` |
| `0x2F` | Unknown | seen |
| `0x30` | Unknown | seen |
| `0x33` | Unknown | seen |
| `0x35` | Unknown | seen |
| `0x37` | Unknown | seen |
| `0x38` | Unknown | `00`, then `52` in the second capture |
| `0x3B` | Unknown | seen |
| `0x3E` | Unknown | seen |

When parsing `0x09` packets, always check byte 6 to identify the specific sub-command. Do not assume all `0x09` packets are noise control.

## Packet Detection

To identify a noise control mode packet:

```
packet.len() > 7 && packet[0..4] == [04 00 04 00] && packet[4] == 0x09 && packet[6] == 0x0D
```

The mode value is at `packet[7]`. This is `aap::noise::is_noise_mode_packet`, and
byte 6 is not optional: validating on the command byte alone accepts every other
sub-command in the table above and reads its value as a mode. The header is the
one every packet after the handshake carries; `aap::decode` checks it once for
every kind of packet, then picks the parser by byte 4.

## Test Results

Tested against AirPods 4 with ANC (firmware A3055), with the Go predecessor of
`examples/noise_probe.rs`.

### Run 1 (initial validation)

| Test | Sent | Audible Change | Response |
|------|------|----------------|----------|
| Off (0x01) | `0400040009000d01000000` | Yes | Confirmation: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | Confirmation: `0x02` (PASS) |
| Transparency (0x03) | `0400040009000d03000000` | Yes | Stale report: `0x02` (caught old state) |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | Stale report: `0x02` (caught old state) |

### Run 2 (improved drain, 4s response window)

| Test | Sent | Audible Change | Response |
|------|------|----------------|----------|
| Off (0x01) | `0400040009000d01000000` | Yes | Confirmation: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |
| Transparency (0x03) | `0400040009000d03000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |

### Run 3 (0x4B packet analysis, 4s response window)

| Test | Sent | Audible Change | 0x4B Response | 0x0D Confirmation |
|------|------|----------------|---------------|-------------------|
| Off (0x01) | `0400040009000d01000000` | Yes | `040004004b0002000109` | Yes: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | `040004004b0002000109` | No |
| Transparency (0x03) | `0400040009000d03000000` | Yes | `040004004b0002000109` | No |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | `040004004b0002000109` | No |

All four 0x4B packets were **byte-identical**: `04 00 04 00 4B 00 02 00 01 09`. The packet contains no mode-specific data — it is a generic "settings family 0x09 was modified" ACK.

### Key Observations

On AirPods 4; AirPods Pro 3 differ, see [Command Acknowledgment](#command-acknowledgment).

1. All four mode commands produce audible mode changes on the AirPods
2. The `0x4B` settings-changed notification is the **only consistent** response (identical for all modes)
3. The `0x0D` confirmation is unreliable — appeared in some runs but not others for the same modes
4. Initial state report (startup dump) reliably contains the current mode
5. The `0x4B` packet is useful as a command ACK but carries no mode information

## References

- [LibrePods AAP Definitions](https://github.com/kavishdevar/librepods/blob/main/docs/AAP%20Definitions.md)
- [LibrePods control commands](https://github.com/kavishdevar/librepods/blob/main/docs/control_commands.md) -
  the 0x09 sub-command table, including 0x34
- [LibrePods GNOME Extension](https://github.com/Anoryth/librepods-gnome)
- [kAirPods (KDE)](https://github.com/can1357/kAirPods)
- [AAP Protocol Definition (Kaitai Struct)](https://github.com/tyalie/AAP-Protocol-Defintion)
