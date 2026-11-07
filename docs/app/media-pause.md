# Pausing the media

How Cymba pauses what plays when a pod leaves an ear, and plays it again once the pod
is back (#167). None of it has been tried on a pair yet: it has run against the fake world
only, in the pipeline tests ([Hardware checks](#hardware-checks)). The places are the
pods' ([protocol/aap/ear.md](../protocol/aap/ear.md#ear-report-0x06)); MPRIS is the
desktop's ([protocol/linux/desktop.md](../protocol/linux/desktop.md#mpris)).

The host's half: taking a pod out pauses what plays, and putting it back plays it again, as
on an iPhone or a Mac. The pods only report where they are. As far as is known they send
no AVRCP pause of their own when a pod comes out - Android needs an app for this too - but
that is believed, not checked ([protocol/aap/ear.md](../protocol/aap/ear.md#open-questions)).

`media::EarPause` decides, pure and taking the time, from the pods' places, whether the
pair's A2DP transport is active, and the players' statuses. `media::Media` carries it out
over D-Bus, in a task of its own: on the session bus for the players, on the system bus for
the transport.

The Preferences switch "Pause when a pod is taken out" turns it off and on
(`Settings::pause_on_ear`, `app.pause-on-ear`), on by default as on an iPhone
([ui.md](ui.md#settings)).

## From the link alone

The places come as `podstate::EarEvent`s (`Coordinator::ear_events`), from the link's
`EarState` once it has both packets: at the link's first reading, and at every change of a
pod's place after it, between the link's `LinkUp` and `LinkGone`
([state.md](state.md#events)). They are told under `inner`'s write lock, before the
snapshot that shows them.

- **Never from the advertisement,** whose in-ear lags and contradicts itself
  ([protocol/aap/ear.md](../protocol/aap/ear.md#why-aap-and-not-the-advertisement), #142).
- **Never from the snapshot.** It falls back to the advertisement's in-ear before the link
  has both packets and after the link goes, and does not say where its in-ear came from.
  So a pair seen only over BLE never pauses anything.
- **Events, not snapshots,** also keep a pod out and back in quickly from being missed.
- **A link's first places pause nothing:** nothing left an ear.

## What pauses and what plays

| | |
|---|---|
| A pod in an ear leaves it, nothing paused | If the pair's A2DP transport is `active`, every MPRIS player that is `Playing` gets `Pause`. The players paused, and which pods were in an ear just before, are remembered. |
| Another pod leaves an ear meanwhile | Nothing: paused already. |
| Every pod worn at the pause back in an ear | Each remembered player that reads `Paused` gets `Play`, and so does one still reading `Playing` within `media::PAUSE_SETTLE` of the pause (below); then every remembered player is forgotten. The transport is not read: it may have gone idle after the pause. |
| A pod goes into the case (`02` or `03`) | The pause is forgotten: the pods were put away, and taking one out and wearing it later plays nothing. From an ear straight into the case in one report pauses and remembers nothing. |
| The link goes | Forgotten. A new link starts afresh. |
| The switch turned off | Forgotten, and nothing paused or played until it is on again. |
| A player refuses `Pause` | It is not remembered: it was not paused, and is nothing to play again. One that does not answer in time is remembered, as the pause may have landed. |

So: both pods worn and one taken out pauses, and back in plays. One pod worn with the
other in the case: the worn one out pauses, back in plays. Both taken out: paused at the
first, played once both are back. A pod that was not worn at the pause need not come back.

**The switch reaches the task on a `watch`,** as a `media::Switch` that counts each turning
off, read at every ear event. So off and on again before the task looked forgets the pause
too.

## Explicit `Pause` and `Play`, never `PlayPause`

A toggle could start what the user paused: sent while nothing plays, or after the user
paused by hand, it would start the media, and a pod coming out must never start anything.

No key is synthesized either. How a press on a pod reaches a player:
[protocol/linux/desktop.md](../protocol/linux/desktop.md#a-press-on-a-pod). A key needs
write access to `/dev/uinput`, and the shell would pick the player.

## Every playing player

A pod coming out means the user stopped listening, so everything `Playing` pauses - also
one that plays to the laptop's speakers while another plays to the pods, as MPRIS does not
say which output a player plays to. Matching a player to the pair's sink (PipeWire stream,
`application.process.id`, the MPRIS name's owner) can come later, if that turns out to
matter.

## The user's action wins

A remembered player that no longer reads `Paused` when the pods are back - played, stopped
or closed by the user - is forgotten and left as it is. A press on a pod meanwhile should
count as this: as far as is understood, it reaches the player, which then no longer reads
`Paused` ([its open question](../protocol/linux/desktop.md#open-questions)).

**Within `media::PAUSE_SETTLE` of the pause** (a guess, not measured), a remembered player
that still reads `Playing` is taken as on its way to `Paused` - a browser presumably hands
a pause to the page first, not checked - and is sent `Play` all the same, which leaves a
playing player as it is.

**The players are read only when the pods are back,** never watched in between. One the
user played by hand reads `Playing`, and is forgotten once `PAUSE_SETTLE` has passed; but
one played and paused again by hand meanwhile reads `Paused`, and plays.

## Calls are left alone

Nothing is paused while the pair's A2DP transport is not `active`: nothing plays to the
pair, or, as assumed for the volume ([volume.md](volume.md#only-while-a2dp-plays-never-in-a-call)),
a call is on. The transport is found as there - under the pair's device, by `Device` and
`UUID` - with `volume::a2dp_active_in`, which takes it whether it has a `Volume` or not:
pausing sets no volume.

## The task

- **Players** are the names starting `org.mpris.MediaPlayer2.` (`ListNames`), each called at
  its owner's unique name (`GetNameOwner`). So a player started again under the same name
  is another, and one owner under two names is one player.
- **Every call goes without auto-start and within `media::CALL_LIMIT`** (a guess), the
  players' side by side. One that hangs is left out, or left as it is, and holds none of
  the others up. Tests give a longer limit (`Media::with_call_limit`, harness only).
- **Only what a decision needs is read** (`media::Needs`). The transport is read only when
  a pod left an ear with nothing paused, and the players only for a pause while the
  transport is active, or to play again. A pod going into the case from out of an ear, or
  out of the case, or a report that changes no pod's place, costs no call. A pod from an
  ear straight into the case is a pod leaving an ear, and calls as a pause does.
- **The task takes no signal,** and connects only for a pod leaving an ear or coming back.
- **Not in a pair's supervisor,** which is aborted wherever it is
  ([aap-links.md](aap-links.md#the-supervisor)).
- **Events go first in its `select!`, the switch next,** so `MediaControl::settled` answers
  for every event told before it. Tests wait on it.

How the fake world plays this through: the fake pods'
[ear detection](../development/fake-world.md#ear-detection) and
[the fake media player](../development/fake-world.md#the-fake-media-player).

## Hardware checks

- With the switch on, playing in Firefox and Spotify: one pod out and back, both out and
  back, one worn with the other in the case.
- How long Firefox and Spotify take to report `Paused` after `Pause`, for `PAUSE_SETTLE`.
- If ear reports still come with the pods' own Automatic Ear Detection turned off on the
  iPhone ([protocol/aap/ear.md](../protocol/aap/ear.md#open-questions)): should the app
  respect that setting?
- A Pro 2 (USB-C) is assumed to behave the same.

The checks of the pods and of the key path:
[protocol/aap/ear.md](../protocol/aap/ear.md#open-questions),
[protocol/linux/desktop.md](../protocol/linux/desktop.md#open-questions).
