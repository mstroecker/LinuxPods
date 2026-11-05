# AAP settings

The `0x09` family: one packet shape for many settings, told apart by a sub-command, and
the `01`/`02` convention of its on/off settings. The shape and the names are LibrePods';
the sub-commands below are those the pods reported in their startup dump on AirPods 4
(2026-02-15) and AirPods Pro 3 (#71), and three that LibrePods names.

The sub-commands the app reads have their own documents: the noise control mode
([noise-control.md](noise-control.md), `aap::noise`) and Conversation Awareness
([conversation-awareness.md](conversation-awareness.md), `aap::conversation`).

Byte offsets count from 0 at the first byte of the packet.

## The packet

```
Offset:  0  1  2  3  4  5  6  7  8  9  10
Hex:     04 00 04 00 09 00 SS VV VV VV VV
         ~~~~~~~~~~~ ~~    ~~ ~~~~~~~~~~~
         Header      Opcode Sub Value, zero-padded
```

- **11 bytes**, the header included; unused value bytes are `00`
  ([LibrePods' control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md)).
- **The same packet goes both ways.** Sent, it sets the setting; sent by the pods, it
  reports it: in the startup dump ([session.md](session.md#initial-state-report)), and
  for the noise control mode whenever it changes ([noise-control.md](noise-control.md#response-behavior)).

⚠️ **Byte 6 is not optional.** Every setting in the family shares byte 4, and several
report values that are valid noise control modes: `0x25`'s `01` reads as Off, `0x26`'s
`02` as Noise Cancellation (Pro 3, #71), and so would Conversation Awareness's `01` and
`02` (`0x28`). Read byte 4 alone, and any of them reads as a mode.

## On and off: `01` and `02`

Most settings that are on or off take `01` for on and `02` for off, not `00`. LibrePods'
table gives it so for allow Off (`0x34`) and Conversation Awareness (`0x28`), among
others. Owns connection (`0x06`) is an exception there: `01` and `00`. Conversation Awareness reported `02` with the setting off and `01` with it on
(Pro 3, #71 and #150), and the window's switch set it with these values (Pro 3,
2026-11-02, [conversation-awareness.md](conversation-awareness.md#the-setting-0x09-sub-command-0x28)).

## Sub-commands

In the order of the sub-command. "Seen" means a report came, its value not recorded. The
Pro 3's dump held 21 settings, in the order [session.md](session.md#initial-state-report)
gives; the AirPods 4's held the 13 with a value in their column.

| Sub-cmd (byte 6) | What | AirPods 4 (2026-02-15) | AirPods Pro 3 (#71) |
|---|---|---|---|
| `0x06` | Owns connection: `01` owns, `00` does not (LibrePods) | - | Not in the dump; no report on any link of a Pro 2 (USB-C) either (#149) |
| `0x0D` | **Noise control mode** - [noise-control.md](noise-control.md) | seen | the mode |
| `0x17` | Unknown | seen | `00` |
| `0x18` | Unknown | seen | `00` |
| `0x1A` | Listening mode configs: a bitmask of the modes in the press-and-hold cycle (LibrePods) | - | Not in the dump |
| `0x1B` | Unknown | seen | `02` |
| `0x1F` | Unknown | `50 50` | `50 50` |
| `0x23` | Unknown | seen | `00` |
| `0x24` | Unknown | `03` | `00 03` |
| `0x25` | Unknown | `01` | `01` |
| `0x26` | Unknown | `01` | `02` |
| `0x28` | **Conversation Awareness**: `01` on, `02` off - [conversation-awareness.md](conversation-awareness.md) | `01` | `02` (`01` in the #150 run) |
| `0x29` | Unknown | - | `02` |
| `0x2C` | Unknown | - | `02` |
| `0x2E` | Unknown | `32` | `32` |
| `0x2F` | Unknown | - | `00` |
| `0x30` | Unknown | - | `01` |
| `0x33` | Unknown | - | `00` |
| `0x34` | **Allow Off as a listening mode**: the gate for Off, `01` allowed, `02` not (LibrePods) - [noise-control.md](noise-control.md#off-has-a-prerequisite-of-its-own) | - | Not in the dump |
| `0x35` | Unknown | seen | `01` |
| `0x37` | Unknown | - | `00` |
| `0x38` | Unknown | - | `00` in #71 capture 1, `52` in capture 2 |
| `0x3B` | Unknown | - | `01` |
| `0x3E` | Unknown | seen | `02` |

Sources for the values:

- **AirPods 4:** the startup dump of the noise control runs ([noise-control.md](noise-control.md#airpods-4-with-anc-2026-02-15)),
  firmware A3055, with the Go predecessor of `examples/noise_probe.rs` (PR #1).
- **AirPods Pro 3:** #71 captures 1 and 2 for `0x0D`, `0x1F`, `0x24`, `0x25`, `0x26`,
  `0x28`, `0x2E` and `0x38`; #71 capture 4 (2026-11-01) for the rest.
- **The names** of `0x06`, `0x1A` and `0x34`: [LibrePods' control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md).
  Neither `0x1A` nor `0x34` was in either dump.

## Open questions

- What the unknown sub-commands are. LibrePods' control command table and AAP
  Definitions name more of the family than the three above; none was checked here.
- Whether the pods report a setting other than the noise control mode when it changes
  ([conversation-awareness.md](conversation-awareness.md#open-questions)).
- AirPods 4: the values of the sub-commands marked "seen" were not recorded.
