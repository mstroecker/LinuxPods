# AAP Ear Detection

How AirPods tell a connected host which pod is in an ear, over AAP: the ear report (opcode `0x06`) and the primary pod (opcode `0x08`). Read by `aap::ear`, and kept per link by `aap::ear::EarState`. What the app does with it beyond the window's icons - pausing the media - is [below](#pausing-the-media).

Captured on AirPods Pro 3 on 2026-11-01, in #142. On AirPods Pro 2 (USB-C) the role switch was captured on 2026-11-02 at d289626 (#71): the same packets in the same order, with longer gaps. That the Pro 2 send the rest the same is assumed, not checked.

## Why AAP and not the advertisement

While a pair is connected, its advertised payload changes seldom and lags its ears: 4 advertisements reached the app in 70 s of the capture, while the ear reports came within 0.5 s of each change. The pods advertise far more often than that (see `ble-proximity-pairing.md`); most likely the scanner's discovery filter, which sets only `Transport`, leaves BlueZ to drop duplicates, so what reaches the app is the payload's changes. Nor do the advertisements agree with each other: each pod advertises from its own side with its own status byte, and with both pods worn two of them read `in_ear=false/true` and `true/false` within 0.6 s.

So while the link is up, in-ear comes from these two packets. The advertisement's in-ear shows only before the link has sent both, and after it goes.

## The packets

Both come unasked, on every change, and once each right after the link comes up, `0x08` first.

### Primary pod: `0x08`

```
04 00 04 00 08 00 PP 00 SS TT
                  ^^ primary pod: 01 left, 02 right
```

10 bytes long.

| Byte | Seen | Reading |
|---|---|---|
| 6 (`PP`) | `01`, `02` | The primary pod: `01` left, `02` right. |
| 7 | `00` | Always `00`. |
| 8 (`SS`) | `01`, `00` | `01` while the secondary pod can be reached, `00` with it in the closed case (once), matching `0x06`'s `03`. |
| 9 (`TT`) | `01`, `00` | `01` in the first packet of a role switch, `00` in a second one 28-291 ms later on the Pro 2, and 31-88 ms later on the Pro 3 in session 2, the only Pro 3 session that shows `TT`. Possibly "switch in progress". `PP` is the new primary in both. |

`PP` agreed every time with the advertisement's primary flag (status bit 5), 13 times across the logs, and with the battery packet, which lists the primary pod first (`03 04 …` with `08 01`, `03 02 …` with `08 02`).

Only `PP` is read. Any value goes in bytes 7-9, and a `PP` other than `01` or `02` is unknown, which leaves the ear report unreadable.

### Ear report: `0x06`

```
04 00 04 00 06 00 PE SE
                  ^^ primary pod's place
                     ^^ secondary pod's place
```

8 bytes long. The report names no pod: it gives the primary pod's place first, so it reads only with the primary pod beside it.

| Place | Meaning |
|---|---|
| `00` | In an ear |
| `01` | Out of the ear, out of the case |
| `02` | In the case, lid open |
| `03` | In the closed case |

- `03` came about 4 s after the lid closed on the secondary pod (4.5 s in capture 4 of #71); at the close itself `00 02` was sent again, unchanged. With `03` the battery packet reports that pod and the case as Disconnected (status `04`, level `00`; #108).
- At a lid change the `0x08` with `SS` changed comes first, then the `0x06`, then the battery. Capture 4 of #71 has them to the millisecond: at the closed case the `0x08` at 19:42:30.160 and the `0x06` with `03` at .192; at the lid opening the `0x08` at 19:43:16.296 and the `0x06` with `02` at .383. Only at a role switch does the `0x06` come first - see below.
- A pod in the case never became primary. With both in the closed case, the pods drop the link.
- Any other byte is unknown, not out: the app keeps the advertisement's in-ear for that pod.

The UI shows in-ear only, so `02` and `03` both read as "not in an ear".

A `0x0D` report of the unchanged noise control mode usually follows an ear report - see `aap-noise-control.md`.

## The role switch

Taking the secondary pod out changes nothing but `0x06`.

When the primary pod comes out, the pod still in an ear becomes primary 0.7-1.2 s later, on both models. A new `0x08` and a `0x06` in the new order then arrive close together, and at every switch captured the `0x06` came first: at all six on the Pro 3, 0.2-31 ms before the `0x08`, and at all eight on the Pro 2, 0.5-87 ms before it.

Both models send the same sequence: the `0x06` in the new order, the `0x08` with `TT` = `01`, the battery in the new order, the `0x08` with `TT` = `00`, then 0.1-0.8 s after the first `0x06` the same `0x06` and battery again. Once, at 16:59:01 on the Pro 3, the `0x06` came a second time between the first `0x08` and the battery.

Read with the old primary in between, that `0x06` swaps the pods: the one just taken out shows in an ear, the one still worn out. The window's render loop wakes at the first change and draws it, so in a pipeline test a watcher shaped like it saw the swap at 4 of 5 switches even with a 1 ms gap. `EarState` therefore holds back a report that puts the primary in an ear and the secondary out, where the last one had the primary out and the secondary in: until the next `0x08`, which it is then read with; until another `0x06`, which replaces it; or for `ROLE_SWITCH_HOLD` (250 ms), after which it is taken as it is.

## The capture

Session 1 (`RUST_LOG=linuxpods=debug`, first 8 bytes of each packet), selected rows:

| UTC | `0x08` before | `0x06` | Then, in arrival order | Reading |
|---|---|---|---|---|
| 16:58:43.6 | `02` (right) | `00 01` | | left out (secondary) |
| 16:58:58.4 | | `00 00` | | left back |
| 16:59:00.6 | `02` (right) | `01 00` | `06 00 01`, `08 01` at 16:59:01.5 | right out (primary); left becomes primary |
| 16:59:07.2 | | `00 00` | | right back |
| 17:04:15.7 | `01` (left) | `00 01` | | right out (secondary) |
| 17:04:19.7 | `01` (left) | `01 00` | `06 00 01`, `08 02` at 17:04:20.6 | left out (primary); right becomes primary |
| 17:16:22.0 | `02` (right) | `01 00` | `06 00 01`, `08 01`, battery `03 04` at 17:16:23.2 | right out (primary); left becomes primary |

Session 2 (a local build logging whole packets), pods in the case, bytes from the opcode on:

| UTC | Packet | Pods |
|---|---|---|
| 17:21:35.0 | `08 00 01 00 01 00` | connect, left primary |
| 17:22:25.1 | `08 00 02 00 01 01`, then `… 01 00` 87 ms later | switch to right |
| 17:22:47.4 | `06 00 00 02` | secondary (left) in the case, lid open |
| 17:22:58.9 | `06 00 00 02` | lid closed: sent again, unchanged |
| 17:23:02.9 | `08 00 02 00 00 00`, `06 00 00 03` | 4 s later: the secondary in the closed case |
| 17:23:17.7 | `08 00 02 00 01 00`, `06 00 00 02` | lid open |
| 17:23:18.1 | `06 00 00 01` | out of the case |
| 17:23:41.8 | `08 00 01 00 01 01`, then `… 01 00` | switch to left |
| 17:24:18.3 | `08 00 02 00 01 01`, then `… 01 00` | switch to right |
| 17:24:20.8 | `06 00 01 02` | primary (right) out, secondary in the case: no switch |
| 17:24:21.3 | | link reset by the pods once both were in the closed case |
| 17:24:40.2 | `08 00 02 00 01 00`, `06 00 00 02` | reconnect: right in an ear, left still in the open case |

Switch timing, `0x06` before the first `0x08`: 17:22:25.062 vs .065, 17:23:41.809 vs .834, 17:24:18.309 vs .311.

The lid rows are timed to 0.1 s only, which does not settle the order of their two packets; they are listed in the order capture 4 of #71 has them, to the millisecond: the `0x08` first at a lid change (see the ear report above).

Session 3, AirPods Pro 2 (USB-C) on 2026-11-02, a plain build of d289626 at `RUST_LOG=linuxpods=trace`: both pods worn, the primary taken out and put back eight times, alternately right and left, 19:27-19:28 UTC. Each removal switched the primary. All eight first `0x06`s were held, and each was read with the `0x08` that followed; none waited out `ROLE_SWITCH_HOLD`.

| Primary out (`06 00 01 00`) | `0x06` in the new order | First `0x08` (`TT` `01`) | Second `0x08` (`TT` `00`) | New primary |
|---|---|---|---|---|
| 19:27:33.004 | +1200 ms | +0.8 ms | +28 ms | left |
| 19:27:43.808 | +697 ms | +3.3 ms | +116 ms | right |
| 19:27:53.200 | +905 ms | +80.0 ms | +83 ms | left |
| 19:28:02.150 | +944 ms | +5.6 ms | +183 ms | right |
| 19:28:11.812 | +960 ms | +87.1 ms | +291 ms | left |
| 19:28:20.677 | +870 ms | +0.5 ms | +80 ms | right |
| 19:28:27.977 | +795 ms | +55.0 ms | +39 ms | left |
| 19:28:36.936 | +851 ms | +2.0 ms | +96 ms | right |

Each column is measured from the one before it.

The `AAP packet (N bytes):` debug line shows the first 8 bytes of a packet; at `trace`, the whole packet follows (key packets redacted).

## Pausing the media

The host's half (#167): taking a pod out pauses what plays, and putting it back plays it again, as on an iPhone or a Mac. The pods only report where they are; as far as is known they send no AVRCP pause of their own when a pod comes out - Android needs an app for this too - but that is not checked here (see below). `media::EarPause` decides, from the pods' places, whether the pair's A2DP transport is active, the players' statuses and the time; `media::Media` carries it out over D-Bus, in a task of its own. The Preferences switch "Pause when a pod is taken out" turns it off and on (`Settings::pause_on_ear`, `app.pause-on-ear`), on by default as on an iPhone.

**From the link alone.** The places come as `podstate::EarEvent`s, from the link's `EarState` once it has both packets, at the link's first reading and at every change of a pod's place after it, with the link coming up and going around them. Never from the advertisement, whose in-ear lags and contradicts itself (above): the snapshot falls back to it before the link has both packets and after the link goes, and does not say where its in-ear came from, so nothing here reads the snapshot. A pair seen only over BLE never pauses anything. Events rather than snapshots also keep a pod out and back in quickly from being missed. A link's first places pause nothing: nothing left an ear.

| | |
|---|---|
| A pod in an ear leaves it, nothing paused | If the pair's A2DP transport is `active`, every MPRIS player that is `Playing` gets `Pause`. The players paused, and which pods were in an ear just before, are remembered. |
| Another pod leaves an ear meanwhile | Nothing: paused already. |
| Every pod worn at the pause back in an ear | Each remembered player that reads `Paused` gets `Play`, and so does one still reading `Playing` within `media::PAUSE_SETTLE` of the pause (below); then every remembered player is forgotten. The transport is not read: it may have gone idle after the pause. |
| A pod goes into the case (`02` or `03`) | The pause is forgotten: the pods were put away, and taking one out and wearing it later plays nothing. From an ear straight into the case in one report pauses and remembers nothing. |
| The link goes | Forgotten. A new link starts afresh. |
| The switch turned off | Forgotten, and nothing paused or played until it is on again - forgotten even if it was turned on again before the task looked, as the task counts each turning off. |
| A player refuses `Pause` | It is not remembered: it was not paused, and is nothing to play again. One that does not answer in time is remembered, as the pause may have landed. |

So: both pods worn and one taken out pauses, and back in plays. One pod worn with the other in the case: the worn one out pauses, back in plays. Both taken out: paused at the first, played once both are back. A pod that was not worn at the pause need not come back.

**Explicit `Pause` and `Play`, never `PlayPause`.** A key press is a toggle: sent while nothing plays, or after the user paused by hand, it would start the media, and a pod coming out must never start anything. As we understand it - not traced here - a press on a pod travels as an AVRCP passthrough that bluetoothd turns into a key on an input device of its own, `<name> (AVRCP)`, which GNOME's media keys service hands to the one player it takes as current over MPRIS. This goes to MPRIS directly, without a key: synthesizing one needs write access to `/dev/uinput`, and the shell would pick the player.

**Every playing player.** A pod coming out means the user stopped listening, so everything `Playing` pauses - one that plays to the laptop's speakers while another plays to the pods too, as MPRIS does not say which output a player plays to. Matching a player to the pair's sink (PipeWire stream, `application.process.id`, the MPRIS name's owner) can come later if that turns out to matter.

**The user's action wins.** A remembered player that no longer reads `Paused` when the pods are back - played, stopped or closed by the user - is forgotten and left as it is. A press on a pod meanwhile should count as this: as far as is understood - the path above, not traced here (see the hardware checks) - it reaches the player, which then no longer reads `Paused`. Within 2 s of the pause (`media::PAUSE_SETTLE`, a guess, not measured), a remembered player that still reads `Playing` is taken as on its way to `Paused` - a browser presumably hands a pause to the page first, not checked - and is sent `Play` all the same, which leaves a playing player as it is. The players are read only when the pods are back, never watched in between: one the user played by hand reads `Playing` and is forgotten once `PAUSE_SETTLE` has passed, but one played and paused again by hand meanwhile reads `Paused`, and plays.

**Calls are left alone.** Nothing is paused while the pair's A2DP transport is not `active`: nothing plays to the pair, or, as assumed for the volume lowered while the user speaks (`aap-conversation-awareness.md`), a call is on. The transport is found as there - under the pair's device, by `Device` and `UUID` - with `volume::a2dp_active_in`, which takes it whether it has a `Volume` or not: pausing sets no volume.

**MPRIS, on the session bus.** Players are the names starting `org.mpris.MediaPlayer2.` (`ListNames`), each called at its owner's unique name (`GetNameOwner`), so a player started again under the same name is another, and one owner under two names is one player. `PlaybackStatus` is read from `org.mpris.MediaPlayer2.Player` at `/org/mpris/MediaPlayer2`, and `Pause` and `Play` called there. Every call goes without auto-start and within `media::CALL_LIMIT` (1 s, a guess), the players' side by side, so one that hangs is left out, or left as it is, and holds none of the others up. The transport is read on the system bus only when a pod left an ear with nothing paused, and the players only for a pause while the transport is active, or to play again: a pod going into the case from out of the ear, or out of the case, or a report that changes no pod's place, costs no call. A pod from an ear straight into the case is a pod leaving an ear, and calls as a pause does.

None of this has been tried on a pair yet (#167): it has run against the fake world only, in the pipeline tests, until the checks below are done.

### Hardware checks

- With the app not running: does taking a pod out pause anything? Does any AVRCP pause come from the pods? (`btmon`, or `evtest` on the `(AVRCP)` input device.)
- A press on a pod: confirm the path above - `evtest` shows the key, and the MPRIS player changes state.
- With the switch on, playing in Firefox and Spotify: one pod out and back, both out and back, one worn with the other in the case.
- How long Firefox and Spotify take to report `Paused` after `Pause`, for `PAUSE_SETTLE`.
- The pods' own Automatic Ear Detection turned off on the iPhone: are ear reports (`0x06`) still sent? If they are, should the app respect that setting?
- A Pro 2 (USB-C) is assumed to behave the same.

## What the fake does

The fake AirPods (`harness/pods.rs`) answer the notification request with the battery, then `0x08`, `0x0C`, `0x0E`, `0x2E`, `0x06` and `0x2E`, as capture 4 of #71 has them, and send a `0x06` on every change of a pod's place after that.

- On a lid change: the `0x08` with `SS` changed, then the `0x06`, then the battery.
- On a role switch: the `0x06` in the new order and the battery, then two `0x08`s - the first 10 ms later with `TT` set, the second 60 ms after it without. The hardware sends the battery between the two `0x08`s instead.
- A pod in the case is in the closed case the moment the lid closes, where the hardware took about 4.5 s.
- With both pods in the closed case, the pods drop the link.
- `SetInEar` and `SetInCase` of `FakeWorld1` move a pod over D-Bus, as `Pods::set_in_ear` and `Pods::set_in_case` do in the pipeline tests.
- A fake MPRIS player (`harness::player`) on the fake world's private session bus - `org.mpris.MediaPlayer2.fakeplayer` under `fakepods` - for the app to pause and play; what the user does in it goes through its `FakePlayer1.SetStatus`, and `Calls` lists every MPRIS method the app called.
