# AAP Conversation Awareness

The `0x28` setting that turns Conversation Awareness on, the speech levels (`0x4B`) the
pods send with it, and the pods' own switch to Transparency while the user speaks. The
setting's report and the levels were checked on AirPods Pro 3 on 2026-11-01 (#150), and
the setting changed from here on a Pro 3 on 2026-11-02. That AirPods Pro 2 (USB-C) behave
the same is assumed, not checked.

Read by `aap::conversation`; what the app keeps and sends:
[app/conversation-awareness.md](../../app/conversation-awareness.md), and how it lowers
the volume on the levels, with what of that has been checked:
[app/volume.md](../../app/volume.md).

Byte offsets count from 0 at the first byte of the packet.

Conversation Awareness lowers the media volume while the user speaks, and raises it again
when they stop ([Apple](https://support.apple.com/en-lamr/guide/airpods/dev6d977ff21/web)).
The pods detect the speech and change their own sound: they switch to Transparency
(#150). Lowering the media is the host's: on an iPhone or a Mac the system does it, on
the levels the pods send (#150's reading, not checked on an Apple device).

**Source:** the packets from [LibrePods](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md)
("Conversational Awareness", "Toggle Conversational Awareness") and its
[control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md)
(`0x28`), for protocol facts only: GitHub detects no license for LibrePods, so no code
is taken from it. What the levels mean comes from the [run](#the-run-pro-3-2026-11-01)
below.

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

Off is `02`, not `00`, as for the family's other on/off settings ([settings.md](settings.md#on-and-off-01-and-02)).

- **Reported in the startup dump**, among the family's twenty-odd settings
  ([settings.md](settings.md)): `02` in every #71 capture (Pro 3), `01` in the #150
  run's dump, the setting already on.
- **Sent, it changes the setting.** That was LibrePods' word, and holds: the window's
  switch changed it on a Pro 3 (2026-11-02, main at 37bf1e5).
- **Not reported again when it changes**, LibrePods says. Not checked here.
- **It persists on the pods.** That it syncs to the user's Apple devices, as allow-Off
  is said to ([noise-control.md](noise-control.md#off-has-a-prerequisite-of-its-own)),
  is a guess.

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

10 bytes, sent by the pods while the setting is on: while the user speaks, and around
every change of noise mode ([below](#levels-08-and-09-around-a-mode-change)). Bytes 5-8
were `00 02 00 01` in every one seen (Pro 3, #150; AirPods 4, [noise-control.md](noise-control.md#runs)).

| Level | Meaning |
|---|---|
| `01`, `02` | Speaking: lower the volume. |
| `03` - `07` | The way back up. Any of them can be skipped. |
| `08` | Back to normal volume. |
| `09` | Idle: the pods are back in the user's mode. |

A level is a target, not a step to count: the pods skip levels, and the ramp from `03`
to `08` took under 100 ms (Pro 3, #150).

LibrePods' two apps disagree on the levels. Its Android app takes `01`/`02` as speech
starting and `06`/`08`/`09` as it stopping. Its Linux app takes `01` as speech starting,
`08` as "disabled", `09` as "enabled", and anything else as stopping. The run below
settles it.

## The pods switch the mode themselves

While the user speaks, the pods switch to Transparency, and back to the user's mode
after. Both are ordinary `0x0D` mode reports. The first, a report of the mode in use,
came 20 ms after level `01` (Pro 3, #150).

How the app takes those reports: [app/noise-control.md](../../app/noise-control.md#the-pods-switch-the-mode-themselves).

## Levels `08` and `09` around a mode change

With the setting on, the levels also frame every change of noise mode: level `08`, the
mode report, then `09`. On a Pro 3 all five changes of the [run](#mode-changes-pro-3-2026-11-01)
were answered so, the bare Off the pods refused included. With the setting off, the same
pair sent no `0x4B` at all (#71).

- `08` and `09` mean "normal volume", not "speech ended".
- They name no mode, so they acknowledge nothing.
- AirPods 4 sent one `09` per change, and their dump had `0x28 01`: the model difference
  recorded in #71 was this setting ([noise-control.md](noise-control.md#0x4b-conversation-awareness-levels-08-and-09)).

## Packet detection

| Packet | Header | Byte 4 | Then | Length |
|---|---|---|---|---|
| The setting | `04 00 04 00` | `09` | `28` at byte 6 | at least 8 bytes |
| A level | `04 00 04 00` | `4B` | `00 02 00 01` at bytes 5-8 | at least 10 bytes |

The setting's byte 6 is not optional: [settings.md](settings.md#the-packet).

## Runs

### The run (Pro 3, 2026-11-01)

A local example (`conversation_awareness`, on 4c01b56, the predecessor of
`examples/conversation_awareness.rs`) opened AAP as the app does and logged every
packet (#150). The setting was already on (`0x28 01` in the dump). Audio playing, three
stretches of speech:

| t (s) | Packet | |
|---|---|---|
| 0.13 | level `09` | right after the dump, unasked |
| 22.03 | level `01` | speech started |
| 22.05 / 22.34 | mode NC, then **Transparency** | the pods switch themselves |
| 22.85 | level `02` | still speaking |
| 28.39 / 28.46 / 28.49 | `03`, `07`, `08` | speech ended: a ramp within 100 ms |
| 28.85 | mode **NC** | the pods switch back |
| 28.87 | level `09` | idle again |

The second stretch went `01` → `06` → `08`, the third `01 02 03 07 08`, each followed by
the mode back and `09`. The stretches lasted 6.4, 3.0 and 4.2 s.

### Mode changes (Pro 3, 2026-11-01)

`examples/noise_probe` on 4c01b56, the setting on, nothing playing (#150). Five mode
changes, Off sent bare among them. Each went like this one:

```
sent  04 00 04 00 09 00 0d 03 00 00 00   Transparency
recv  04 00 04 00 4b 00 02 00 01 08      level 08
recv  04 00 04 00 09 00 0d 03 00 00 00   mode report
recv  04 00 04 00 4b 00 02 00 01 09      level 09
```

Ten `0x4B`s, all `08` or `09`, one pair per change; nothing else.

## References

- [LibrePods AAP Definitions](https://github.com/librepods-org/librepods/blob/main/docs/AAP%20Definitions.md) - Conversational Awareness, Toggle Conversational Awareness
- [LibrePods control commands](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md) - `0x28`
- [Apple: noise control settings on AirPods Pro](https://support.apple.com/en-lamr/guide/airpods/dev6d977ff21/web)

## Open questions

- The setting changed from here: does the phone's switch follow?
- The setting changed on a phone while the pair is linked here: does a `0x28` report come?
- Speech with nothing playing: are levels sent at all?
- A mode set from the app during speech: what do the pods report, and which mode do they go back to?
- Pro 2 (USB-C): the setting and the levels, assumed to be as on the Pro 3.
