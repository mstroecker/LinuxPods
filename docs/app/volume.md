# Volume

How LinuxPods lowers the pair's volume while the user speaks, and puts it back.

The volume the app lowers while the user speaks goes through BlueZ's `MediaTransport1.Volume`, which was set by hand on the Pro 3 on 2026-11-01; the app's lowering to a fifth and its fade back were checked on the Pro 3 in that run, and the rest of it has run against the fake world only - see [Lowering the volume](#lowering-the-volume) and [Hardware checks](#hardware-checks).

The speech levels this acts on: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#speech-levels-0x4b).

## Lowering the volume

The host's half: while the user speaks, the app lowers the media's volume, as an iPhone or a Mac does on the same levels. `volume::Ducking` decides, from the levels, the transport's volume and the time; `volume::Volume` carries it out over D-Bus, in a task of its own.

**The pair's own A2DP volume, through BlueZ.** `org.bluez.MediaTransport1.Volume`, the AVRCP absolute volume of the pair's A2DP transport ([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)); never the default output, never a volume in software, no subprocess. PipeWire's sink follows it: [protocol/linux/desktop.md](../protocol/linux/desktop.md#the-volume).

**Looked up every time.** BlueZ makes a transport for each A2DP setup, at a path of its own under the device ([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)) - so it is never kept: each level that may set something, and each step of a fade, reads BlueZ's objects (`GetManagedObjects`, without auto-start) and takes the transport under the pair's device whose `Device` is that device and whose `UUID` is A2DP's, with its `State` and `Volume`. No transport means nothing plays to the pair: there is nothing to lower. No call waits longer than `volume::CALL_LIMIT` (1 s).

| | |
|---|---|
| First `01`/`02` | The volume then is remembered, and the transport set to 20% of it, in one step - 1 at least, unless it was 0. What lag there is is the pods': speech is heard before `01` is sent. |
| Later `01`/`02` | Nothing: it is lowered. |
| First level of `03` or above | A fade back to the remembered volume starts: 1.5 s, a step every 100 ms, whatever levels come meanwhile. The pods ramp `03` to `08` in under 100 ms ([the run](../protocol/aap/conversation-awareness.md#the-run-pro-3-2026-11-01)), and the #150 prototype jumped straight back. |
| `01`/`02` during the fade | Down again at once, to 20% of the volume remembered, not of the faded one. |
| `08`, `09` with nothing lowered | Nothing, and no call to BlueZ: they frame every mode change. |
| The link goes | The volume is put back at once, closed here or lost; with the transport gone too, once it is back - see below. |
| The app quits | The same, before the runtime goes, for 2 s at most (`volume::RESTORE_LIMIT`, a choice): on Quit, and on the first SIGTERM - a logout - or SIGINT; a second exits at once. Not on SIGKILL, nor for a transport that is gone. |

20% is fixed, as in the prototype [the run](../protocol/aap/conversation-awareness.md#the-run-pro-3-2026-11-01) used; LibrePods lowers to 20% on Linux and 43% by default on Android. A Preferences setting may come later.

**What is ours.** What a `Set` of `Volume` does, and when the property changes: [protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume).

So the app never waits for an answer, nor reads one back. Whenever it reads the transport - at a level, a fade's step, a look for a transport that went, the link going, the quit - a reading counts as ours if it is within `volume::READ_BACK_SLACK` (4 of 127, a guess) of any volume set this stretch, or is the volume before while the lowering is unanswered: until a reading of another volume has come since the lowering. A slow answer then never looks like the user's - the transport still reading the volume before at `02` or `03` leaves the stretch lowered and the fade starting from the volume set - and a late answer, rounded or not, lands on a volume that is ours wherever the fade is.

What that costs:

- A user change of no more than 4 steps from a volume the app set this stretch counts as the app's, and the fade goes on over it. #150's run had PipeWire's sink follow the transport in proportion - 32 as 25%, 20 as 16% - so GNOME's volume keys, 5% a press, would move it about 6 of 127, above the slack. That is an inference from that run; the keys were not tried.
- The volume before, while the lowering is unanswered, counts as ours too: a user who puts the volume straight back before the app has read the answer gets the fade from the lowered volume anyway.
- A lowering still unanswered when the link goes is waited for, as a transport that went is; one unanswered when the app quits is not put back, as setting the volume the transport still reads would send nothing. Pods that answered nothing through a whole stretch and its fade would end a step short of the volume before. How long they take is untried.

**A volume the user changed is theirs.** The volume is put back, and a fade goes on, only while the transport reads a volume that is ours; the lowering itself sets from any volume. Moved meanwhile - GNOME's slider, the keys, the pods' own controls - the remembered volume is forgotten and the rest of the stretch sets nothing; a stretch after that is lowered from the user's volume.

**Put back later.** bluetoothd keeps the volume on the device and starts its next transport at it ([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)), so a volume lowered when the transport goes - the pair disconnected or out of range, the pods in the case, A2DP set up again - would come back lowered in every later session. So the volume remembered is kept. With the link up, the transport is looked for each second for 30 s from when it went (`volume::PUT_BACK_CHECK`, `volume::PUT_BACK_WINDOW`: guesses at how long A2DP takes to come back, not watched); after that, and from the link going, it waits for the pair's next link (`SpeechEvent::LinkUp`), from which it is looked for for the same 30 s, or for the next level, which reads the transport anyway. Found reading a volume that is ours, it is put back; another is the user's, or another host's, and kept.

**Only while A2DP plays, never in a call.** A stretch is lowered only while the pair's A2DP transport is `active` - streaming, and acquired by the audio server. Not `idle` or `pending`: lowering a transport that streams nothing changes nothing the user hears, and a song started meanwhile would start lowered. That is also what keeps a call alone: whatever runs the headset profile - PipeWire's own HFP backend, its default, or BlueZ itself - is assumed to leave A2DP not `active`, released or gone, while it has the pair; that is not read in any source, nor watched during a call here (#150). A headset-profile transport is not looked for: while that assumption holds it would tell nothing more, and a call that left A2DP `active` has not been seen. A call that comes while the volume is lowered does not stop it coming back: the fade goes on with A2DP idle, and with A2DP gone the volume is put back once it returns, as above.

**Events, not snapshots.** The levels reach the volume as `podstate::SpeechEvent`s, every one in the order the pods sent it, between the link coming up and its going. The snapshot keeps only the latest level, and a short stretch's `01` could be gone before a slow consumer looked.

How the fake world plays this through: [development/fake-world.md](../development/fake-world.md#the-volume-lowered-while-the-user-speaks).

## Hardware checks

Checked on a Pro 3 on 2026-11-02, with main at 37bf1e5: the window's switch changes the setting, and speech while audio plays lowers the volume to a fifth at once and fades it back once the user stops.

Still to do:

- A volume changed while lowered - GNOME's slider - is kept, and the fade does not run. Does GNOME's slider follow the transport at all?
- A call with PipeWire's own HFP backend: does the A2DP transport stay, `idle`, or go, and for how long? Either way speech in the call should leave the volume alone, and A2DP come back at the volume it had.
- A logout while the volume is lowered: is it put back?
- Disconnected in the middle of speech, then connected again: is the volume put back once A2DP is set up again?
- A2DP set up again in the middle of a stretch - a codec switch, say: is the volume put back on the new transport?
- A volume of 1 or 2 before speech: lowered to 1, and back.

The checks of the levels and of the pods' answer to a set: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#open-questions), [protocol/linux/bluez.md](../protocol/linux/bluez.md#open-questions).
