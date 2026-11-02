# AAP Conversation Awareness

Conversation Awareness lowers the media volume while the user speaks and raises it again when they stop. The pods detect the speech and change their own sound - they switch to Transparency - but lowering the media is the host's job: on an iPhone or a Mac the OS does it on the levels the pods send. This document describes the setting that turns it on, those levels, what the pods do while the user speaks, and how the app lowers the volume on the levels. Read by `aap::conversation` and `volume`.

**Status:** the setting's report in the startup dump and the levels checked on AirPods Pro 3 on 2026-11-01 (#150), with `examples/conversation_awareness.rs`'s predecessor and `examples/noise_probe.rs`. That AirPods Pro 2 (USB-C) behave the same is assumed, not checked. The `0x4B` packets AirPods 4 sent after every mode change (`aap-noise-control.md`) are the same levels. The command that changes the setting is LibrePods'; the window's switch changed it on the Pro 3 on 2026-11-02, with main at 37bf1e5. The volume the app lowers while the user speaks goes through BlueZ's `MediaTransport1.Volume`, which was set by hand on the Pro 3 on 2026-11-01; the app's lowering to a fifth and its fade back were checked on the Pro 3 in that run, and the rest of it has run against the fake world only - see [Lowering the volume](#lowering-the-volume) and [Hardware checks](#hardware-checks).

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

Disabled is `02`, not `00`, as for the other boolean settings of the family (`0x34` allow-Off among them). The packet reports the setting in the startup dump, among the family's twenty-odd settings (`aap-noise-control.md`): every #71 capture had `02`, and the #150 run's dump `01`, the setting already on. That the same packet, sent, changes the setting was LibrePods' word, and holds: the window's switch changed it on a Pro 3 (2026-11-02, main at 37bf1e5).

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
- `Command::ConversationAwareness` sends the packet to turn it on or off (taken by a Pro 3, 2026-11-02), through `Coordinator::command` like the noise mode. The Features group's switch sends it, and is sensitive while the device shown is on AAP and has reported the setting.
- The last level is kept on the link and in the snapshot (`PodState::speech_level`), and logged at debug. Every level also goes, as an event, to what lowers the volume: `Coordinator::speech_events` - see below.

## Lowering the volume

The host's half: while the user speaks, the app lowers the media's volume, as an iPhone or a Mac does on the same levels. `volume::Ducking` decides, from the levels, the transport's volume and the time; `volume::Volume` carries it out over D-Bus, in a task of its own.

**The pair's own A2DP volume, through BlueZ.** `org.bluez.MediaTransport1.Volume` (`q`, 0-127, writable, signaled), the AVRCP absolute volume of the pair's A2DP transport. It goes to the pods themselves; never the default output, never a volume in software, no subprocess. Checked on a Pro 3 on 2026-11-01 (#150): the transport's `Volume` set from 32 to 20 and back with `busctl`, and PipeWire's sink followed within 0.5 s (25% to 16% to 25%). GNOME's slider, which follows the sink, was not watched. BlueZ's documentation (`org.bluez.MediaTransport(5)`) says `Volume` is writable only by the client that acquired the transport; bluetoothd checks no owner for it (`profiles/audio/transport.c`), and `busctl` was not the owner.

**Looked up every time.** BlueZ makes a transport for each A2DP setup, at a path of its own under the device - `<device>/sep1/fd<n>`, `n` the first number free from the count of all bluetoothd's transports (`media_transport_create` in BlueZ 5.87's `transport.c`) - so it is never kept: each level that may set something, and each step of a fade, reads BlueZ's objects (`GetManagedObjects`, without auto-start) and takes the transport under the pair's device whose `Device` is that device and whose `UUID` is A2DP's, with its `State` and `Volume`. No transport means nothing plays to the pair: there is nothing to lower. No call waits longer than `volume::CALL_LIMIT` (1 s).

| | |
|---|---|
| First `01`/`02` | The volume then is remembered, and the transport set to 20% of it, in one step - 1 at least, unless it was 0. What lag there is is the pods': speech is heard before `01` is sent. |
| Later `01`/`02` | Nothing: it is lowered. |
| First level of `03` or above | A fade back to the remembered volume starts: 1.5 s, a step every 100 ms, whatever levels come meanwhile. The pods ramp `03` to `08` in under 100 ms (the run above), and the #150 prototype jumped straight back. |
| `01`/`02` during the fade | Down again at once, to 20% of the volume remembered, not of the faded one. |
| `08`, `09` with nothing lowered | Nothing, and no call to BlueZ: they frame every mode change. |
| The link goes | The volume is put back at once, closed here or lost; with the transport gone too, once it is back - see below. |
| The app quits | The same, before the runtime goes, for 2 s at most (`volume::RESTORE_LIMIT`, a choice): on Quit, and on the first SIGTERM - a logout - or SIGINT; a second exits at once. Not on SIGKILL, nor for a transport that is gone. |

20% is fixed, as in the prototype the run above used; LibrePods lowers to 20% on Linux and 43% by default on Android. A Preferences setting may come later.

**What is ours.** A `Set` of `Volume` changes nothing at once, nor necessarily to the volume set. In BlueZ 5.87's source, a host-as-source transport's setter (`transport_a2dp_src_set_volume` in `transport.c`) does nothing for the volume the transport holds, and otherwise sends the volume to the pods over AVRCP (`avrcp_set_volume`) and changes nothing; the property changes once the pods answer (`avrcp_handle_set_volume` in `profiles/audio/avrcp.c`), to the volume they report, which may be rounded. That is a reading of the source: how long the pods take, and whether they round, is untried here; the Pro 3 read 20 back as set with `busctl`, some time after the set.

So the app never waits for an answer, nor reads one back. Whenever it reads the transport - at a level, a fade's step, a look for a transport that went, the link going, the quit - a reading counts as ours if it is within `volume::READ_BACK_SLACK` (4 of 127, a guess) of any volume set this stretch, or is the volume before while the lowering is unanswered: until a reading of another volume has come since the lowering. A slow answer then never looks like the user's - the transport still reading the volume before at `02` or `03` leaves the stretch lowered and the fade starting from the volume set - and a late answer, rounded or not, lands on a volume that is ours wherever the fade is.

What that costs:

- A user change of no more than 4 steps from a volume the app set this stretch counts as the app's, and the fade goes on over it. #150's run had PipeWire's sink follow the transport in proportion - 32 as 25%, 20 as 16% - so GNOME's volume keys, 5% a press, would move it about 6 of 127, above the slack. That is an inference from that run; the keys were not tried.
- The volume before, while the lowering is unanswered, counts as ours too: a user who puts the volume straight back before the app has read the answer gets the fade from the lowered volume anyway.
- A lowering still unanswered when the link goes is waited for, as a transport that went is; one unanswered when the app quits is not put back, as setting the volume the transport still reads would send nothing. Pods that answered nothing through a whole stretch and its fade would end a step short of the volume before. How long they take is untried.

**A volume the user changed is theirs.** The volume is put back, and a fade goes on, only while the transport reads a volume that is ours; the lowering itself sets from any volume. Moved meanwhile - GNOME's slider, the keys, the pods' own controls - the remembered volume is forgotten and the rest of the stretch sets nothing; a stretch after that is lowered from the user's volume.

**Put back later.** bluetoothd keeps the volume on the device and starts its next transport at it (`btd_device_set_volume` in `transport.c`), so a volume lowered when the transport goes - the pair disconnected or out of range, the pods in the case, A2DP set up again - would come back lowered in every later session. So the volume remembered is kept. With the link up, the transport is looked for each second for 30 s from when it went (`volume::PUT_BACK_CHECK`, `volume::PUT_BACK_WINDOW`: guesses at how long A2DP takes to come back, not watched); after that, and from the link going, it waits for the pair's next link (`SpeechEvent::LinkUp`), from which it is looked for for the same 30 s, or for the next level, which reads the transport anyway. Found reading a volume that is ours, it is put back; another is the user's, or another host's, and kept.

**Only while A2DP plays, never in a call.** A stretch is lowered only while the pair's A2DP transport is `active` - streaming, and acquired by the audio server. Not `idle` or `pending`: lowering a transport that streams nothing changes nothing the user hears, and a song started meanwhile would start lowered. That is also what keeps a call alone: whatever runs the headset profile - PipeWire's own HFP backend, its default, or BlueZ itself - is assumed to leave A2DP not `active`, released or gone, while it has the pair; that is not read in any source, nor watched during a call here (#150). A headset-profile transport is not looked for: while that assumption holds it would tell nothing more, and a call that left A2DP `active` has not been seen. A call that comes while the volume is lowered does not stop it coming back: the fade goes on with A2DP idle, and with A2DP gone the volume is put back once it returns, as above.

**Events, not snapshots.** The levels reach the volume as `podstate::SpeechEvent`s, every one in the order the pods sent it, between the link coming up and its going. The snapshot keeps only the latest level, and a short stretch's `01` could be gone before a slow consumer looked.

The fake world plays this through: a fake BlueZ with A2DP transports, a call as A2DP going idle, a pair that answers a volume late and rounded if asked (`SetVolumeAnswer`), the user's volume, and fake pods that speak - see `.claude/skills/fake-world/SKILL.md` and the pipeline tests.

## Hardware checks

Checked on a Pro 3 on 2026-11-02, with main at 37bf1e5: the window's switch changes the setting, and speech while audio plays lowers the volume to a fifth at once and fades it back once the user stops.

Still to do:

- The setting changed from here: does the phone's switch follow?
- The setting changed on a phone while the pair is linked here: does a `0x28` report come?
- Speech with nothing playing: are levels sent at all?
- A mode set from the app during speech: what do the pods report, and which mode do they go back to?
- Do the pods answer a set with another volume than the one set, and by how much? No run of its own: the app's debug log (`RUST_LOG=linuxpods::volume=debug`) of a stretch of speech shows each volume set beside what the transport read then, and the example what it read back after each set.
- A volume changed while lowered - GNOME's slider - is kept, and the fade does not run. Does GNOME's slider follow the transport at all?
- A call with PipeWire's own HFP backend: does the A2DP transport stay, `idle`, or go, and for how long? Either way speech in the call should leave the volume alone, and A2DP come back at the volume it had.
- A logout while the volume is lowered: is it put back?
- Disconnected in the middle of speech, then connected again: is the volume put back once A2DP is set up again?
- A2DP set up again in the middle of a stretch - a codec switch, say: is the volume put back on the new transport?
- A volume of 1 or 2 before speech: lowered to 1, and back.

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
