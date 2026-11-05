# AAP noise control

The packet that sets the noise control mode (Off, Noise Cancellation, Transparency,
Adaptive), the `0x34` gate before Off, and what the pods report around a change. The
packet is LibrePods'. It was checked on AirPods 4 with ANC (firmware A3055, 2026-02-15,
PR #1), on AirPods Pro 3 and Pro 2 (USB-C) (2026-09-12, d46feaa), and on a Pro 3 in the
#71 captures. What answers a change differs by model and by a setting.

Read by `aap::noise`; how the app sets the mode and takes the reports:
[app/noise-control.md](../../app/noise-control.md).
`cargo run --example noise_probe -- <MAC>` reproduces the exchanges below.

Byte offsets count from 0 at the first byte of the packet.

## Command Packet

```
Offset:  0  1  2  3  4  5  6  7  8  9  10
Hex:     04 00 04 00 09 00 0D MM 00 00 00
         ~~~~~~~~~~  ~~    ~~ ~~
         Header      Cmd   Sub Mode
```

A setting of the `0x09` family, sub-command `0x0D` ([settings.md](settings.md)): 11
bytes, the mode at byte 7, bytes 8-10 `00`.

### Mode Values

| Byte | Mode | Description |
|------|------|-------------|
| `0x01` | Off | Noise control disabled |
| `0x02` | Noise Cancellation | Active Noise Cancellation (ANC) |
| `0x03` | Transparency | Pass-through audio from microphones |
| `0x04` | Adaptive | Automatically adjusts to environment |

The values are in [LibrePods' control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md).
All four changed the mode audibly on AirPods 4 ([runs](#runs)), and all four took on a
Pro 3 (#71 capture 1).

### Full Packet Examples

```
Off:          04 00 04 00 09 00 0D 01 00 00 00
ANC:          04 00 04 00 09 00 0D 02 00 00 00
Transparency: 04 00 04 00 09 00 0D 03 00 00 00
Adaptive:     04 00 04 00 09 00 0D 04 00 00 00
```

## Prerequisites

The features packet (`0x4D`), sent as a session comes up: [session.md](session.md#prerequisites-the-features-packet).

### Off has a prerequisite of its own

A bare `0x0D 01` is refused on AirPods Pro 3 (0x2027) and AirPods Pro 2 (USB-C) (0x2024):
the pods play an error chime and stay in the mode they were in. The other three modes
work without it (2026-09-12, d46feaa). A Pro 3 refused a bare Off again on 2026-11-01
(#150). AirPods 4 with ANC took a bare Off (PR #1). Which firmware brought the gate is
not known.

Sub-command `0x34`, "Allow Off Option for Listening Mode" in LibrePods' control command
table, is the gate. Sent immediately before the mode, with no delay between the two, it
lets Off through (d46feaa):

```
Allow Off:  04 00 04 00 09 00 34 01 00 00 00
Forbid Off: 04 00 04 00 09 00 34 02 00 00 00
```

Disabled is `02`, not `00`: [settings.md](settings.md#on-and-off-01-and-02).

Two guesses, written with the gate's fix (d46feaa) and checked nowhere: Apple gates Off
because loud sound reduction does not apply while noise control is off; and the setting
is the same switch the user sees on an iPhone, persisting on the pods and syncing back
to the user's other Apple devices.

When the app sends it: [app/noise-control.md](../../app/noise-control.md#allowing-off).

## Response Behavior

### Command Acknowledgment

What answers a mode change depends on the model and on the Conversation Awareness
setting. Nothing answers it reliably on every model:

| Model | `0x0D` echo of the new mode | `0x4B` | Source |
|---|---|---|---|
| AirPods 4 with ANC, Conversation Awareness on | For perhaps one change in four | Level `09` after every change | [Runs](#runs), #150 |
| AirPods Pro 3, Conversation Awareness off | Every change, about 1 s after it | None | #71 capture 1 |
| AirPods Pro 3, Conversation Awareness on | Every change | Level `08` before the echo, `09` after it | #150 |

The levels around a change: [conversation-awareness.md](conversation-awareness.md#levels-08-and-09-around-a-mode-change).

How the app records the mode without waiting for the echo: [app/noise-control.md](../../app/noise-control.md#recorded-not-confirmed).

**Stale reports.** A `0x0D` report can carry the mode a change replaced. Switched from
Noise Cancellation to Off, a Pro 3 reported `0D 02` 0.33 s later, and `0D 01` only at
1.4 s (#71 capture 1). On AirPods 4, Transparency and Adaptive were each followed by a
report of the old mode, `02`, with no echo of the new one after it ([run 1](#run-1-initial-validation)).

How the app holds these reports off: [app/noise-control.md](../../app/noise-control.md#stale-reports).

### 0x4B: Conversation Awareness levels 08 and 09

⚠️ **The `0x4B` after a mode change is no acknowledgment.** After every mode change,
AirPods 4 sent `04 00 04 00 4B 00 02 00 01 09`, and a Pro 3 in #71 sent none. It was
long read here as a generic settings-changed notification - a count of one, and the
`0x09` family that changed. It is Conversation Awareness's speech level `09`, byte for
byte, which the pods send only with that setting on (#150):

- it is identical whichever mode was set: it names neither the sub-command nor the mode;
- it comes only with Conversation Awareness on, and then also while the user speaks;
- so it acknowledges nothing, and is no sign that a command was processed.

Why AirPods 4 sent it and the Pro 3 in #71 did not: [conversation-awareness.md](conversation-awareness.md#levels-08-and-09-around-a-mode-change).
The AirPods 4 runs below have one `09` per change, and the variant `…01 08` once (run 1,
ANC); they stay as observed.

### Initial State Report

On every AAP connection the pods report the current mode in the startup dump, among the
`0x09` settings:

```
04 00 04 00 09 00 0D 04 00 00 00
                     ^^ the current mode (Adaptive here)
```

The dump held the mode in every run: AirPods 4 ([runs](#runs)) and Pro 3 (#71).

When the dump comes: [session.md](session.md#initial-state-report).

### Ear Detection Reports

A Pro 3 reports every change of a pod's place, unasked, in an ear report (`0x06`): see
[ear.md](ear.md) for its layout and the primary pod packet (`0x08`) it reads with
(#71 capture 1, #142).

- A `0x0D` report of the unchanged mode usually follows an ear report.
- With exactly one pod in an ear, the pods briefly report `0D 01` (Off), and go back to
  the mode about 50 ms later.

Neither is a change of mode by the user (Pro 3, #71 capture 1).

### Unsolicited Mode Changes

The pods report the mode unasked "when the noise control mode is changed (either by a stem
long press or by the connected device", LibrePods' AAP Definitions say of the AirPods Pro
2. That a change made on another device - an iPhone, an Apple Watch - is reported so is a
guess.

With Conversation Awareness on, the pods also switch to Transparency themselves while the
user speaks, and back after, reported the same way ([conversation-awareness.md](conversation-awareness.md#the-pods-switch-the-mode-themselves)).

How the app takes these reports: [app/noise-control.md](../../app/noise-control.md#reports-from-elsewhere).

## Settings Command Family (0x09)

The `0x09` sub-commands, of which `0x0D` is one: [settings.md](settings.md).

## Packet Detection

A noise control mode packet has the header `04 00 04 00`, `09` at byte 4, `0D` at byte
6, and at least 8 bytes. The mode is byte 7. Why byte 6 matters:
[settings.md](settings.md#the-packet).

## Runs

### AirPods 4 with ANC (2026-02-15)

Firmware A3055, with the Go predecessor of `examples/noise_probe.rs` (PR #1). The
pair's Conversation Awareness was on ([settings.md](settings.md#sub-commands)).

#### Run 1 (initial validation)

| Test | Sent | Audible Change | Response |
|------|------|----------------|----------|
| Off (0x01) | `0400040009000d01000000` | Yes | Confirmation: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | Confirmation: `0x02` (PASS) |
| Transparency (0x03) | `0400040009000d03000000` | Yes | Stale report: `0x02` (caught old state) |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | Stale report: `0x02` (caught old state) |

#### Run 2 (improved drain, 4s response window)

| Test | Sent | Audible Change | Response |
|------|------|----------------|----------|
| Off (0x01) | `0400040009000d01000000` | Yes | Confirmation: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |
| Transparency (0x03) | `0400040009000d03000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | Only `0x4B` packet, no `0x0D` confirmation |

#### Run 3 (0x4B packet analysis, 4s response window)

| Test | Sent | Audible Change | 0x4B Response | 0x0D Confirmation |
|------|------|----------------|---------------|-------------------|
| Off (0x01) | `0400040009000d01000000` | Yes | `040004004b0002000109` | Yes: `0x01` (PASS) |
| ANC (0x02) | `0400040009000d02000000` | Yes | `040004004b0002000109` | No |
| Transparency (0x03) | `0400040009000d03000000` | Yes | `040004004b0002000109` | No |
| Adaptive (0x04) | `0400040009000d04000000` | Yes | `040004004b0002000109` | No |

All four `0x4B` packets were byte-identical, `04 00 04 00 4B 00 02 00 01 09`: level `09`,
with nothing of the mode in it.

#### What the AirPods 4 runs show

1. All four mode commands change the mode audibly.
2. Level `09` (`0x4B`) is the only consistent answer, identical for all modes, and only
   because Conversation Awareness was on.
3. The `0x0D` confirmation is unreliable: it came in some runs and not in others, for
   the same modes.
4. The startup dump held the current mode in every run.
5. The `0x4B` level carries no mode, and acknowledges nothing.

### #71 capture 1 (Pro 3)

Build 06a426d, the app at `RUST_LOG=linuxpods=trace` beside `btmon`, Conversation
Awareness off (`0x28 02` in the dump):

| Mode | Sent | `0x0D` echo | `0x4B` |
|---|---|---|---|
| Adaptive | `0D 04` | +1.1 s | none |
| Noise Cancellation | `0D 02` | +1.0 s | none |
| Off | `34 01` then `0D 01`, same ms | stale `0D 02` at +0.33 s, then `0D 01` at +1.4 s | none |
| Transparency | `0D 03` | +1.0 s | none |

The run with the setting on: [conversation-awareness.md](conversation-awareness.md#mode-changes-pro-3-2026-11-01).

## References

- [LibrePods AAP Definitions](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md)
- [LibrePods control commands](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md) -
  the 0x09 sub-command table, including 0x34
- [LibrePods GNOME Extension](https://github.com/Anoryth/librepods-gnome)
- [kAirPods (KDE)](https://github.com/can1357/kAirPods)
- [AAP Protocol Definition (Kaitai Struct)](https://github.com/tyalie/AAP-Protocol-Defintion)

## Open questions

- Other models: AirPods Pro (1st gen), AirPods Max (PR #1's test plan) - the mode packet,
  the gate before Off, and what answers a change.
- A mode changed on an iPhone or a Watch while the pair is linked here: the `0x0D` report
  that comes, checked on a Pro 3.
- Whether `0x34` syncs to the user's other Apple devices, and whether it is reported when
  it changes.
- Which firmware brought the gate before Off, and whether AirPods 4 have it now.
- The Pro 2 (USB-C)'s answers to a change, assumed to be the Pro 3's.
