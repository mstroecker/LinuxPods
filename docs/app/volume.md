# Volume

How LinuxPods lowers the pair's volume while the user speaks, and puts it back. The
speech levels it acts on are the pods'
([protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#speech-levels-0x4b));
the transport's volume is bluetoothd's
([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)). The
lowering and the fade ran on a Pro 3 on 2026-11-02; the rest has run against the fake
world only ([Hardware checks](#hardware-checks)).

## Lowering the volume

The host's half (#150): while the user speaks, the app lowers the media's volume, as an
iPhone or a Mac does on the same levels. `volume::Ducking` decides, pure and taking the
time, from the levels and the transport's volume. `volume::Volume` carries it out over
D-Bus, in a task of its own.

**The pair's own A2DP volume, through BlueZ:** `org.bluez.MediaTransport1.Volume`, the
AVRCP absolute volume of the pair's A2DP transport. Never the default output, never
PipeWire, never a volume in software, no subprocess. PipeWire's sink follows it
([protocol/linux/desktop.md](../protocol/linux/desktop.md#the-volume)); GNOME's slider,
which follows the sink, was not watched.

**Looked up every time.** BlueZ makes a transport at a new path for each A2DP setup, so its
path is never kept. Each level that may set something, and each step of a fade, reads
BlueZ's objects (`GetManagedObjects`) and takes the transport under the pair's device
whose `Device` is that device and whose `UUID` is A2DP's, with its `State` and `Volume`
(`volume::a2dp_in`). No transport means nothing plays to the pair: nothing to lower.

| | |
|---|---|
| First `01`/`02` | The volume then is remembered, and the transport set to `volume::LOWERED_PERCENT` of it, in one step: 1 at least, unless it was 0. What lag there is is the pods': speech is heard before `01` is sent. |
| Later `01`/`02` | Nothing: it is lowered. |
| First level of `03` or above | A fade back to the remembered volume starts, over `volume::FADE` in steps of `volume::FADE_STEP`, whatever levels come meanwhile. The pods ramp `03` to `08` in under 100 ms ([the run](../protocol/aap/conversation-awareness.md#the-run-pro-3-2026-11-01)), and the #150 prototype jumped straight back. |
| `01`/`02` during the fade | Down again at once, to the lowered volume of the volume remembered, not of the faded one. |
| `08`, `09` with nothing lowered | Nothing, and no call to BlueZ: they frame every mode change. |
| The link goes | The volume is put back at once, closed here or lost; with the transport gone too, once it is back: [Put back later](#put-back-later). |
| The app quits | The same, before the runtime goes, for `volume::RESTORE_LIMIT` at most (a choice): on Quit, and on the first SIGTERM - a logout - or SIGINT; a second exits at once. Not on SIGKILL, nor for a transport that is gone. |

`LOWERED_PERCENT` is fixed, at the 20% the prototype of [the run](../protocol/aap/conversation-awareness.md#the-run-pro-3-2026-11-01)
used. LibrePods lowers to 20% on Linux, and 43% by default on Android. A Preferences
setting may come later.

## What is ours

A `Set` of `Volume` changes the property only once the pods answer, to the volume they
report, which may be rounded
([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)).

So the app never waits for an answer, nor reads one back. Whenever it reads the transport -
at a level, a fade's step, a look for a transport that went, the link going, the quit - a
reading counts as ours if:

- it is within `volume::READ_BACK_SLACK` (a guess) of any volume set this stretch, or
- it is the volume before, while the lowering is unanswered: until a reading of another
  volume has come since the lowering.

**Never compare with the last value set alone,** and never take the volume before as the
user's while the lowering is unanswered. A slow answer then never looks like the user's:
the transport still reading the volume before at `02` or `03` leaves the stretch lowered,
and the fade starting from the volume set. A late answer, rounded or not, lands on a
volume that is ours wherever the fade is.

What that costs:

- **A user change within the slack of a volume the app set** counts as the app's, and the
  fade goes on over it. PipeWire's sink followed the transport in proportion in #150's run
  ([protocol/linux/desktop.md](../protocol/linux/desktop.md#runs)), so GNOME's volume keys,
  5% a press, would move it about 6 of 127, above the slack. That is an inference from that
  run; the keys were not tried.
- **The volume before counts as ours** while the lowering is unanswered. A user who puts the
  volume straight back before the app has read the answer gets the fade from the lowered
  volume anyway.
- **A lowering still unanswered when the link goes** is waited for, as a transport that went
  is. One unanswered when the app quits is not put back, as setting the volume the
  transport still reads would send nothing. Pods that answered nothing through a whole
  stretch and its fade would end a step short of the volume before. How long they take is
  untried.

The fake BlueZ answers late and rounded (`SetVolumeAnswer`), so the tests meet both.

## The user's change wins

The volume is put back, and a fade goes on, only while the transport reads a volume that is
ours; the lowering itself sets from any volume. Moved meanwhile - GNOME's slider, the keys,
the pods' own controls - the remembered volume is forgotten, and the rest of the stretch
sets nothing. A stretch after that is lowered from the user's volume.

## Put back later

bluetoothd keeps the volume on the device and starts its next transport at it
([protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume)). So a volume
lowered when the transport goes - the pair disconnected or out of range, the pods in the
case, A2DP set up again - would come back lowered in every later session. So the volume
remembered is kept:

| | The transport is looked for |
|---|---|
| The link up, the transport gone | Each `volume::PUT_BACK_CHECK`, for `volume::PUT_BACK_WINDOW` from when it went (guesses at how long A2DP takes to come back, not watched) |
| Past that window, or the link gone | At the pair's next link (`SpeechEvent::LinkUp`), for the same window, or at the next level, which reads the transport anyway |

Found reading a volume that is ours, it is put back. Another is the user's, or another
host's, and kept.

**When the link goes:** `SpeechEvent::LinkGone`, told by `drop_link` under `inner`'s
write lock, after the link's last level ([state.md](state.md#events)).

**Not in a pair's supervisor:** that is aborted wherever it is
([aap-links.md](aap-links.md#the-supervisor)). The volume's task outlives every link.

## Only while A2DP plays, never in a call

A stretch is lowered only while the pair's A2DP transport is `active`: streaming, and
acquired by the audio server.

- **Not `idle` or `pending`:** lowering a transport that streams nothing changes nothing the
  user hears, and a song started meanwhile would start lowered.
- **Calls are left alone, assumed.** Whatever runs the headset profile - PipeWire's own HFP
  backend, its default, or BlueZ itself - is assumed to leave A2DP not `active`, released
  or gone, while it has the pair. That is not read in any source, nor watched during a call
  here (#150); calls have run in the fake world only.
- **No headset-profile transport is looked for:** while that assumption holds it would tell
  nothing more, and a call that left A2DP `active` has not been seen.
- **A call that comes while the volume is lowered** does not stop it coming back: the fade
  goes on with A2DP idle, and with A2DP gone the volume is put back once it returns.

## The task

- **Levels as events, not snapshots** (`Coordinator::speech_events`): the watch keeps the
  latest alone, and a short stretch's `01` could be gone before a consumer looked
  ([state.md](state.md#events)).
- **Every BlueZ call goes without auto-start** ([bluez.md](bluez.md#never-starting-bluetoothd)),
  and each call and the bus connection are bounded by `volume::CALL_LIMIT`.
- **The task takes no signal.** It calls BlueZ only for a speech level, a fade step, a look
  for a transport that went, a link going or the quit. It connects when a call needs it,
  and again after a connection failed, so it has no retry loop to back off.
- **Events go first in its `select!`,** so `VolumeControl::settled` answers for every event
  told before it. Tests wait on it, never on a sleep.

How the fake world plays this through:
[development/fake-world.md](../development/fake-world.md#the-volume-lowered-while-the-user-speaks).

## Hardware checks

Checked on a Pro 3 on 2026-11-02, with main at 37bf1e5: speech while audio plays lowers
the volume to a fifth at once, and fades it back once the user stops.

Still to do:

- A volume changed while lowered - GNOME's slider - is kept, and the fade does not run.
  Does GNOME's slider follow the transport at all?
- A call with PipeWire's own HFP backend: does the A2DP transport stay, `idle`, or go, and
  for how long? Either way speech in the call should leave the volume alone, and A2DP come
  back at the volume it had.
- A logout while the volume is lowered: is it put back?
- Disconnected in the middle of speech, then connected again: is the volume put back once
  A2DP is set up again?
- A2DP set up again in the middle of a stretch - a codec switch, say: is the volume put
  back on the new transport?
- A volume of 1 or 2 before speech: lowered to 1, and back.

The checks of the levels and of the pods' answer to a set:
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#open-questions),
[protocol/linux/bluez.md](../protocol/linux/bluez.md#open-questions).
