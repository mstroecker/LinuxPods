# Pausing the media

How LinuxPods pauses what plays when a pod leaves an ear, and plays it again once the pod is back.

The host's half (#167): taking a pod out pauses what plays, and putting it back plays it again, as on an iPhone or a Mac. The pods only report where they are; as far as is known they send no AVRCP pause of their own when a pod comes out - Android needs an app for this too - but that is not checked here (see [protocol/aap/ear.md](../protocol/aap/ear.md#open-questions)). `media::EarPause` decides, from the pods' places, whether the pair's A2DP transport is active, the players' statuses and the time; `media::Media` carries it out over D-Bus, in a task of its own. The Preferences switch "Pause when a pod is taken out" turns it off and on (`Settings::pause_on_ear`, `app.pause-on-ear`), on by default as on an iPhone.

The pods' places, as the ear report gives them: [protocol/aap/ear.md](../protocol/aap/ear.md#ear-report-0x06).

**From the link alone.** The places come as `podstate::EarEvent`s, from the link's `EarState` once it has both packets, at the link's first reading and at every change of a pod's place after it, with the link coming up and going around them. Never from the advertisement, whose in-ear lags and contradicts itself ([protocol/aap/ear.md](../protocol/aap/ear.md#why-aap-and-not-the-advertisement)): the snapshot falls back to it before the link has both packets and after the link goes, and does not say where its in-ear came from, so nothing here reads the snapshot. A pair seen only over BLE never pauses anything. Events rather than snapshots also keep a pod out and back in quickly from being missed. A link's first places pause nothing: nothing left an ear.

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

**Explicit `Pause` and `Play`, never `PlayPause`.** A key press is a toggle: sent while nothing plays, or after the user paused by hand, it would start the media, and a pod coming out must never start anything.

How a press on a pod reaches a player: [protocol/linux/desktop.md](../protocol/linux/desktop.md#a-press-on-a-pod).

This goes to MPRIS directly, without a key: synthesizing one needs write access to `/dev/uinput`, and the shell would pick the player.

**Every playing player.** A pod coming out means the user stopped listening, so everything `Playing` pauses - one that plays to the laptop's speakers while another plays to the pods too, as MPRIS does not say which output a player plays to. Matching a player to the pair's sink (PipeWire stream, `application.process.id`, the MPRIS name's owner) can come later if that turns out to matter.

**The user's action wins.** A remembered player that no longer reads `Paused` when the pods are back - played, stopped or closed by the user - is forgotten and left as it is. A press on a pod meanwhile should count as this: as far as is understood - the path in [protocol/linux/desktop.md](../protocol/linux/desktop.md#a-press-on-a-pod), not traced here (see [its open question](../protocol/linux/desktop.md#open-questions)) - it reaches the player, which then no longer reads `Paused`. Within 2 s of the pause (`media::PAUSE_SETTLE`, a guess, not measured), a remembered player that still reads `Playing` is taken as on its way to `Paused` - a browser presumably hands a pause to the page first, not checked - and is sent `Play` all the same, which leaves a playing player as it is. The players are read only when the pods are back, never watched in between: one the user played by hand reads `Playing` and is forgotten once `PAUSE_SETTLE` has passed, but one played and paused again by hand meanwhile reads `Paused`, and plays.

**Calls are left alone.** Nothing is paused while the pair's A2DP transport is not `active`: nothing plays to the pair, or, as assumed for the volume lowered while the user speaks ([volume.md](volume.md#lowering-the-volume)), a call is on. The transport is found as there - under the pair's device, by `Device` and `UUID` - with `volume::a2dp_active_in`, which takes it whether it has a `Volume` or not: pausing sets no volume.

**MPRIS, on the session bus.** Players are the names starting `org.mpris.MediaPlayer2.` (`ListNames`), each called at its owner's unique name (`GetNameOwner`), so a player started again under the same name is another, and one owner under two names is one player. `PlaybackStatus` is read from `org.mpris.MediaPlayer2.Player` at `/org/mpris/MediaPlayer2`, and `Pause` and `Play` called there. Every call goes without auto-start and within `media::CALL_LIMIT` (1 s, a guess), the players' side by side, so one that hangs is left out, or left as it is, and holds none of the others up. The transport is read on the system bus only when a pod left an ear with nothing paused, and the players only for a pause while the transport is active, or to play again: a pod going into the case from out of the ear, or out of the case, or a report that changes no pod's place, costs no call. A pod from an ear straight into the case is a pod leaving an ear, and calls as a pause does.

None of this has been tried on a pair yet (#167): it has run against the fake world only, in the pipeline tests, until the checks below are done.

## Hardware checks

- With the switch on, playing in Firefox and Spotify: one pod out and back, both out and back, one worn with the other in the case.
- How long Firefox and Spotify take to report `Paused` after `Pause`, for `PAUSE_SETTLE`.
- If ear reports still come with the pods' own Automatic Ear Detection turned off on the iPhone ([protocol/aap/ear.md](../protocol/aap/ear.md#open-questions)): should the app respect that setting?
- A Pro 2 (USB-C) is assumed to behave the same.

The checks of the pods and of the key path: [protocol/aap/ear.md](../protocol/aap/ear.md#open-questions), [protocol/linux/desktop.md](../protocol/linux/desktop.md#open-questions).

How the fake world plays this through: [development/fake-world.md](../development/fake-world.md#ear-detection).
