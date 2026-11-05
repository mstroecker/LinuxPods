# AAP noise control

The packet that sets the noise control mode, the `0x34` gate before Off, and what the pods report around a change.

## Overview

AirPods noise control modes (Off, ANC, Transparency, Adaptive) can be switched via the Apple Accessory Protocol (AAP) over L2CAP PSM 4097. This document describes the verified packet format and response behavior.

**Status:** Verified against AirPods 4 with ANC (firmware A3055) on 2026-02-15, against AirPods Pro 3 and AirPods Pro Gen 2 on 2026-09-12, and against AirPods Pro 3 in the #71 captures. `cargo run --example noise_probe -- <MAC>` reproduces the exchange below.

**Source:** Packet format originally from [LibrePods](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md) reverse engineering, validated independently.

How the app sets the mode and reads the reports: [app/noise-control.md](../../app/noise-control.md).

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

The features packet (`0x4D`) every connection sends first: [session.md](session.md#prerequisites-the-features-packet).

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
persists on the device and syncs back to their other Apple devices.

When the app sends it: [app/noise-control.md](../../app/noise-control.md#off-has-a-prerequisite-of-its-own).

Identifier and values from [LibrePods' control command
table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md).

## Response Behavior

### Command Acknowledgment

What answers a mode change depends on the model and on the Conversation Awareness setting, and nothing answers it reliably on all of them:

- **AirPods 4** (the test results below): a `0x0D` echo of the new mode for perhaps one change in four. Every change also brought a `0x4B` naming neither the sub-command nor the mode: Conversation Awareness's level `09`, as their setting was on - see [below](#0x4b-conversation-awareness-levels-08-and-09).
- **AirPods Pro 3** (#71): a `0x0D` echo of every change, about 1 s after it. With Conversation Awareness off, as it was there, no `0x4B` at all; with it on (#150), level `08` before the echo and `09` after it.

| Mode | Sent | `0x0D` echo | `0x4B`, Conversation Awareness off |
|---|---|---|---|
| Adaptive | `0D 04` | +1.1 s | none |
| Noise Cancellation | `0D 02` | +1.0 s | none |
| Off | `34 01` then `0D 01`, same ms | stale `0D 02` at +0.33 s, then `0D 01` at +1.4 s | none |
| Transparency | `0D 03` | +1.0 s | none |

How the app records the mode without waiting for the echo: [app/noise-control.md](../../app/noise-control.md#command-acknowledgment).

**Stale reports.** A `0x0D` report can carry the mode a change replaced. Switched from Noise Cancellation to Off, a pair reported `0D 02` 0.33 s later and `0D 01` only at 1.4 s (#71).

How the app holds these reports off: [app/noise-control.md](../../app/noise-control.md#stale-reports).

### 0x4B: Conversation Awareness levels 08 and 09

After every noise control command, AirPods 4 sent an identical `0x4B` packet, and AirPods Pro 3 in #71 sent none. It was long read here as a generic settings-changed notification - a count of one, and the `0x09` family that changed. It is not: it is Conversation Awareness's speech level `09`, byte for byte ([conversation-awareness.md](conversation-awareness.md), #150).

The levels around a mode change, and the run that showed them: [conversation-awareness.md](conversation-awareness.md#levels-08-and-09-around-a-mode-change).

The AirPods 4 records below have one `09` per change, and the variant `…01 08` once (run 1, ANC test); those stay as observed.

**Key properties:**
- The packet is **identical** whichever mode was set: it names neither the sub-command nor the mode
- It comes only with Conversation Awareness on, and then also while the user speaks
- So it is no acknowledgment of a mode change, nor a sign that a command was processed: `aap::decode` reads it as a speech level, `Message::SpeechLevel`

### Initial State Report

On AAP connection, the AirPods send a burst of `0x09` settings packets during the startup dump. One of these is the current noise control mode:

```
04 00 04 00 09 00 0D 04 00 00 00
                     ^^ Current mode (Adaptive in this example)
```

This startup report is reliable and should be parsed to initialize the local noise control state.

When the dump comes: [session.md](session.md#initial-state-report).

### Ear Detection Reports

AirPods Pro 3 report every change of a pod's place, unasked, in an ear report (`0x06`) - see [ear.md](ear.md) for its layout and the primary pod packet (`0x08`) it reads with. A `0x0D` report of the unchanged mode usually follows. With exactly one pod in an ear, the pods briefly report `0D 01` (Off) and go back to the mode about 50 ms later. Neither is a mode change of the user's.

### Unsolicited Mode Changes

When the noise control mode is changed from another device (e.g., iPhone, Apple Watch), the AirPods send an unsolicited `0x0D` packet with the new mode. Parse these to keep local state synchronized.

With Conversation Awareness on, the pods also switch to Transparency themselves while the user speaks, and back after, reported the same way ([conversation-awareness.md](conversation-awareness.md)).

How the app takes these reports: [app/noise-control.md](../../app/noise-control.md#unsolicited-mode-changes).

## Settings Command Family (0x09)

The `0x09` sub-commands, of which `0x0D` is one: [settings.md](settings.md).

## Packet Detection

To identify a noise control mode packet:

```
packet.len() > 7 && packet[0..4] == [04 00 04 00] && packet[4] == 0x09 && packet[6] == 0x0D
```

The mode value is at `packet[7]`. This is `aap::noise::is_noise_mode_packet`, and
byte 6 is not optional: validating on the command byte alone accepts every other
sub-command in the table in [settings.md](settings.md) and reads its value as a mode. The header is the
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

All four 0x4B packets were **byte-identical**: `04 00 04 00 4B 00 02 00 01 09`. The packet contains no mode-specific data: it is Conversation Awareness's level `09`, which the pair sent after every change because its setting was on - see [above](#0x4b-conversation-awareness-levels-08-and-09).

### Key Observations

On AirPods 4, with Conversation Awareness on; AirPods Pro 3 differ, see [Command Acknowledgment](#command-acknowledgment).

1. All four mode commands produce audible mode changes on the AirPods
2. Level `09` of Conversation Awareness (`0x4B`) is the **only consistent** response (identical for all modes), and only because the setting was on
3. The `0x0D` confirmation is unreliable — appeared in some runs but not others for the same modes
4. Initial state report (startup dump) reliably contains the current mode
5. The `0x4B` level carries no mode information, and acknowledges nothing

## References

- [LibrePods AAP Definitions](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md)
- [LibrePods control commands](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md) -
  the 0x09 sub-command table, including 0x34
- [LibrePods GNOME Extension](https://github.com/Anoryth/librepods-gnome)
- [kAirPods (KDE)](https://github.com/can1357/kAirPods)
- [AAP Protocol Definition (Kaitai Struct)](https://github.com/tyalie/AAP-Protocol-Defintion)
