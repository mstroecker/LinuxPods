# AAP Conversation Awareness

Conversation Awareness lowers the media volume while the user speaks and raises it again when they stop. The pods detect the speech and change their own sound - they switch to Transparency - but lowering the media is the host's job: on an iPhone or a Mac the OS does it on the levels the pods send. This document describes the setting that turns it on, those levels, and what the pods do while the user speaks. Read by `aap::conversation`.

**Status:** the setting's report in the startup dump and the levels checked on AirPods Pro 3 on 2026-11-01 (#150), with `examples/conversation_awareness.rs`'s predecessor and `examples/noise_probe.rs`. That AirPods Pro 2 (USB-C) behave the same is assumed, not checked. The `0x4B` packets AirPods 4 sent after every mode change (`aap-noise-control.md`) are the same levels. The command that changes the setting is LibrePods'; it has not been sent to a pair from here yet.

**Source:** the packets from [LibrePods](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md) ("Conversational Awareness", "Toggle Conversational Awareness") and its [control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md) (`0x28`), for protocol facts only: GitHub detects no license for LibrePods, so no code is taken from it. What the levels mean comes from the run below.

## The setting: `0x09` sub-command `0x28`

```
Offset:  0  1  2  3  4  5  6  7  8  9  10
Hex:     04 00 04 00 09 00 28 VV 00 00 00
         ~~~~~~~~~~  ~~    ~~ ~~
         Header      Cmd   Sub Value
```

| Value | Meaning |
|---|---|
| `01` | On |
| `02` | Off |

Disabled is `02`, not `00`, as for the other boolean settings of the family (`0x34` allow-Off among them). The packet reports the setting in the startup dump, among the family's twenty-odd settings (`aap-noise-control.md`): every #71 capture had `02`, and the #150 run's dump `01`, the setting already on. That the same packet, sent, changes the setting is LibrePods' word: the report is confirmed, the command not yet tried from here.

LibrePods says the setting is not reported again when it changes. It is a persistent setting of the pods', most likely synced to the user's Apple devices as allow-Off is.

So the app records the setting optimistically, as it does the noise mode: `Coordinator::set_conversation_awareness` records it once the packet is away. A report that does come is taken whenever it comes. A change made on a phone while the pair is linked here may never reach the app, and then the switch shows the old setting until the next connection's dump: accepted, until the hardware checks below say otherwise.

### Prerequisite: the features packet

```
Feature Enable: 04 00 04 00 4D 00 FF 00 00 00 00 00 00 00
```

The `0x4D` packet every link sends as it comes up (`aap::noise::ENABLE_FEATURES`) is what makes Conversation Awareness work while audio plays; without it, LibrePods says, only while nothing plays.

## Speech levels: `0x4B`

```
Offset:  0  1  2  3  4  5  6  7  8  9
Hex:     04 00 04 00 4B 00 02 00 01 LL
         ~~~~~~~~~~  ~~ ~~~~~~~~~~~ ~~
         Header      Cmd            Level
```

10 bytes, sent by the pods with the setting on: while the user speaks, and around every change of noise mode (below). Bytes 5-8 were `00 02 00 01` in every one seen, and `aap::conversation::is_level_packet` checks them: a `0x4B` shaped otherwise is no level it knows.

| Level | Meaning |
|---|---|
| `01`, `02` | Speaking: lower the volume. |
| `03` - `07` | The way back up. Any of them can be skipped. |
| `08` | Back to normal volume. |
| `09` | Idle: the pods are back in the user's mode. |

A level is a target, not a step to count: the pods skip levels, and the ramp from `03` to `08` took under 100 ms. `aap::conversation::Speech` reads them so.

LibrePods' two apps disagree on the levels: its Android app takes `01`/`02` as speech starting and `06`/`08`/`09` as it stopping, its Linux app `01` as speech starting, `08` as "disabled", `09` as "enabled" and anything else as stopping. The run below settles it.

## The run (Pro 3, 2026-11-01)

A local example opened AAP as the app does and logged every packet. The setting was already on (`0x28 01` in the dump). Audio playing, three stretches of speech:

| t (s) | Packet | |
|---|---|---|
| 0.13 | level `09` | right after the dump, unasked |
| 22.03 | level `01` | speech started |
| 22.05 / 22.34 | mode NC, then **Transparency** | the pods switch themselves |
| 22.85 | level `02` | still speaking |
| 28.39 / 28.46 / 28.49 | `03`, `07`, `08` | speech ended: a ramp within 100 ms |
| 28.85 | mode **NC** | the pods switch back |
| 28.87 | level `09` | idle again |

The second stretch went `01` → `06` → `08`, the third `01 02 03 07 08`, each followed by the mode back and `09`. The stretches lasted 6.4, 3.0 and 4.2 s.

## The pods switch the mode themselves

While the user speaks, the pods switch to Transparency, and back to the user's mode after: reported as ordinary `0x0D` mode reports, the first a report of the mode in use 20 ms after level `01`. The app takes them as it takes any report, so the window and the tray show Transparency while the user talks. That is the mode the pods are in.

A mode set from the app during a stretch is held as any command's is (`aap::noise::ModeState`): a report of Transparency within 2 s of it is taken for a stale one and dropped, and the next report - the echo, or the switch back after the speech - settles what is shown. What the pods do with a mode set during speech is not recorded; the fake pods go back to it after the stretch.

## Levels `08` and `09` around a mode change

With the setting on, the levels also frame every change of noise mode. Checked on the Pro 3 (2026-11-01, `examples/noise_probe.rs`, nothing playing): every mode change was answered level `08`, the mode report, then `09` - all five, the bare Off the pods refused included:

```
sent  04 00 04 00 09 00 0d 03 00 00 00   Transparency
recv  04 00 04 00 4b 00 02 00 01 08      level 08
recv  04 00 04 00 09 00 0d 03 00 00 00   mode report
recv  04 00 04 00 4b 00 02 00 01 09      level 09
```

Ten `0x4B`s, all `08` or `09`, one pair per change; nothing else. With the setting off (#71) the same pair sent none. So `08` and `09` mean "normal volume", not "speech ended", and name no mode: they acknowledge nothing.

AirPods 4 sent one `09` per change (`aap-noise-control.md`), and their dump had `0x28 01`: the model difference recorded in #71 was this setting.

## In the app

- `aap::decode` reads the setting as `Message::ConversationAwareness` and a level as `Message::SpeechLevel`.
- The setting is kept on the link and shown in the snapshot (`PodState::conversation_awareness`); it goes with the link, as the noise mode does.
- `Command::ConversationAwareness` sends LibrePods' packet to turn it on or off, not yet confirmed on a pair, through `Coordinator::command` like the noise mode. The Features group's switch sends it, and is sensitive while the device shown is on AAP and has reported the setting.
- The last level is kept on the link and in the snapshot (`PodState::speech_level`), and logged at debug. Nothing acts on it yet.

## Lowering the volume

Planned, #150: from the levels of a pair with a link, lower that pair's own A2DP volume - `org.bluez.MediaTransport1.Volume`, its AVRCP absolute volume - at once on the first `01`/`02`, and fade it back to what it was from the first level of `03` or above; never during a call.

## Hardware checks still to do

- `28 01` and `28 02` sent from here: does the pair take them? `cargo run --example conversation_awareness -- <MAC> --on` against a pair whose dump says off, speaking during the run; then connect again and read the dump. And does the phone's switch follow?
- The setting changed on a phone while the pair is linked here: does a `0x28` report come?
- Speech with nothing playing: are levels sent at all?
- A mode set from the app during speech: what do the pods report, and which mode do they go back to?

## Packet detection

```
setting: packet.len() > 7 && packet[0..4] == [04 00 04 00] && packet[4] == 0x09 && packet[6] == 0x28
level:   packet.len() > 9 && packet[0..4] == [04 00 04 00] && packet[4] == 0x4B && packet[5..9] == [00 02 00 01]
```

The setting's byte 6 is not optional: the `0x09` family's other settings, the noise control mode among them, share byte 4, and its `01` and `02` read as Off and Noise Cancellation as well as on and off.

## References

- [LibrePods AAP Definitions](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md) - Conversational Awareness, Toggle Conversational Awareness
- [LibrePods control commands](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md) - `0x28`
- [Apple: noise control settings on AirPods Pro](https://support.apple.com/en-lamr/guide/airpods/dev6d977ff21/web)
