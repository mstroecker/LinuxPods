# AAP Noise Control Protocol

## Overview

AirPods noise control modes (Off, ANC, Transparency, Adaptive) can be switched via the Apple Accessory Protocol (AAP) over L2CAP PSM 4097. This document describes the verified packet format, response behavior, and integration notes.

**Status:** Verified against AirPods 4 with ANC (firmware A3055) on 2026-02-15 using `debug_aap_noise_control`.

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

## Response Behavior

### Command Acknowledgment

The AirPods do **not** reliably echo a noise control confirmation packet (`0x0D`) after a mode change. Instead, every mode change consistently produces a `0x4B` settings-changed notification. Occasionally a `0x0D` echo also arrives, but it cannot be relied upon.

**Integration note:** Treat noise control commands as fire-and-forget. Update local state optimistically after sending — do not wait for a `0x0D` confirmation packet.

### 0x4B Settings-Changed Notification

After every noise control command, the AirPods respond with an identical `0x4B` packet:

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
- It is the **only consistent response** to a noise control command
- Variant `...01 08` was also observed once (run 1, ANC test) — byte 9 = `0x08` instead of `0x09`, suggesting other setting families also use this notification format
- Useful as a lightweight ACK ("command was processed") but not for determining which mode was set

### Initial State Report

On AAP connection, the AirPods send a burst of `0x09` settings packets during the startup dump. One of these is the current noise control mode:

```
04 00 04 00 09 00 0D 04 00 00 00
                     ^^ Current mode (Adaptive in this example)
```

This startup report is reliable and should be parsed to initialize the local noise control state.

### Unsolicited Mode Changes

When the noise control mode is changed from another device (e.g., iPhone, Apple Watch), the AirPods send an unsolicited `0x0D` packet with the new mode. Parse these to keep local state synchronized.

## Settings Command Family (0x09)

The `0x09` command byte is a family of settings sub-commands, not exclusive to noise control. Observed sub-commands during connection startup:

| Sub-cmd (byte 6) | Notes |
|-------------------|-------|
| `0x0D` | **Noise control mode** |
| `0x17` | Unknown |
| `0x18` | Unknown |
| `0x1B` | Unknown |
| `0x1F` | Unknown (data: `50 50`) |
| `0x23` | Unknown |
| `0x24` | Unknown (data: `03`) |
| `0x25` | Unknown (data: `01`) |
| `0x26` | Unknown (data: `01`) |
| `0x28` | Unknown (data: `01`) |
| `0x2E` | Unknown (data: `32`) |
| `0x35` | Unknown |
| `0x3E` | Unknown |

When parsing `0x09` packets, always check byte 6 to identify the specific sub-command. Do not assume all `0x09` packets are noise control.

## Packet Detection

To identify a noise control mode packet:

```
len(packet) >= 8 && packet[4] == 0x09 && packet[6] == 0x0D
```

The mode value is at `packet[7]`.

## Test Results

Tested with `cmd/debug_aap_noise_control` against AirPods 4 with ANC (MAC: `C4:B3:49:D8:40:52`, firmware A3055).

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

1. All four mode commands produce audible mode changes on the AirPods
2. The `0x4B` settings-changed notification is the **only consistent** response (identical for all modes)
3. The `0x0D` confirmation is unreliable — appeared in some runs but not others for the same modes
4. Initial state report (startup dump) reliably contains the current mode
5. The `0x4B` packet is useful as a command ACK but carries no mode information

## References

- [LibrePods AAP Definitions](https://github.com/kavishdevar/librepods/blob/main/AAP%20Definitions.md)
- [LibrePods GNOME Extension](https://github.com/Anoryth/librepods-gnome)
- [kAirPods (KDE)](https://github.com/can1357/kAirPods)
- [AAP Protocol Definition (Kaitai Struct)](https://github.com/tyalie/AAP-Protocol-Defintion)
