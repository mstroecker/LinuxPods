# AAP Conversation Awareness

The `0x28` setting that turns Conversation Awareness on, the speech levels (`0x4B`) the pods send with it, and the pods' own switch to Transparency.

Conversation Awareness lowers the media volume while the user speaks and raises it again when they stop. The pods detect the speech and change their own sound - they switch to Transparency - but lowering the media is the host's job: on an iPhone or a Mac the OS does it on the levels the pods send. This document describes the setting that turns it on, those levels, and what the pods do while the user speaks. Read by `aap::conversation`.

**Status:** the setting's report in the startup dump and the levels checked on AirPods Pro 3 on 2026-11-01 (#150), with `examples/conversation_awareness.rs`'s predecessor and `examples/noise_probe.rs`. That AirPods Pro 2 (USB-C) behave the same is assumed, not checked. The `0x4B` packets AirPods 4 sent after every mode change ([noise-control.md](noise-control.md)) are the same levels. The command that changes the setting is LibrePods'; the window's switch changed it on the Pro 3 on 2026-11-02, with main at 37bf1e5.

What has been checked of the volume the app lowers: [app/volume.md](../../app/volume.md).

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

Disabled is `02`, not `00`, as for the other boolean settings of the family (`0x34` allow-Off among them). The packet reports the setting in the startup dump, among the family's twenty-odd settings ([settings.md](settings.md)): every #71 capture had `02`, and the #150 run's dump `01`, the setting already on. That the same packet, sent, changes the setting was LibrePods' word, and holds: the window's switch changed it on a Pro 3 (2026-11-02, main at 37bf1e5).

LibrePods says the setting is not reported again when it changes. It is a persistent setting of the pods', most likely synced to the user's Apple devices as allow-Off is.

How the app records the setting: [app/conversation-awareness.md](../../app/conversation-awareness.md#the-setting).

### Prerequisite: the features packet

The features packet (`0x4D`) and what Conversation Awareness needs it for: [session.md](session.md#prerequisites-the-features-packet).

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

While the user speaks, the pods switch to Transparency, and back to the user's mode after: reported as ordinary `0x0D` mode reports, the first a report of the mode in use 20 ms after level `01`.

How the app takes those reports: [app/noise-control.md](../../app/noise-control.md#the-pods-switch-the-mode-themselves).

## Levels `08` and `09` around a mode change

With the setting on, the levels also frame every change of noise mode. Checked on the Pro 3 (2026-11-01, `examples/noise_probe.rs`, nothing playing): every mode change was answered level `08`, the mode report, then `09` - all five, the bare Off the pods refused included:

```
sent  04 00 04 00 09 00 0d 03 00 00 00   Transparency
recv  04 00 04 00 4b 00 02 00 01 08      level 08
recv  04 00 04 00 09 00 0d 03 00 00 00   mode report
recv  04 00 04 00 4b 00 02 00 01 09      level 09
```

Ten `0x4B`s, all `08` or `09`, one pair per change; nothing else. With the setting off (#71) the same pair sent none. So `08` and `09` mean "normal volume", not "speech ended", and name no mode: they acknowledge nothing.

AirPods 4 sent one `09` per change ([noise-control.md](noise-control.md#0x4b-conversation-awareness-levels-08-and-09)), and their dump had `0x28 01`: the model difference recorded in #71 was this setting.

## In the app

What the app reads, keeps and sends: [app/conversation-awareness.md](../../app/conversation-awareness.md#in-the-app).

## Lowering the volume

How the app lowers the volume on the levels: [app/volume.md](../../app/volume.md#lowering-the-volume).

## Open questions

- The setting changed from here: does the phone's switch follow?
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
