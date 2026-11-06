# State

How the coordinator keeps each pair's state: the merge of AAP and BLE per pair, the
snapshots and events it hands out, the BLE cache and its clock, and the lid. All of it
is `podstate`; the reasons below come from bugs, each with its issue.

## Per pair

The coordinator merges AAP and BLE **per pair**, keyed by the pair's **real** MAC. An
advertisement carries only a random address until it is identified
([identification.md](identification.md)), so everything below runs after
identification.

**A link supersedes BLE for its own pair only** (`Inner::supersedes_ble`). A global
pause, one link silencing BLE for everything, blanked out every other pair.

**From the link's first battery packet, not from link-up** (#62). Hidden from link-up,
the pair showed nothing until that packet came - about a second on one pair - and its
GNOME Settings battery was withdrawn and exported again on every reconnect.

### What a snapshot shows

`Inner::snapshot` merges each pair's state from these:

| Field | From |
|---|---|
| Levels, charging | The link's reading once it has one, else the cached advertisement |
| In-ear | The link's ear state once it has both packets, else the cached advertisement: [in-ear.md](in-ear.md) |
| Lid, connection state | The cached advertisement: AAP carries neither, as far as is known |
| Model, color | The identity the link holds, taken from the cached advertisement as the link comes up and refreshed by every one after, so it outlives the cache entry |
| Noise mode, Conversation Awareness, speech level, the pods' name | The link alone: [noise-control.md](noise-control.md), [conversation-awareness.md](conversation-awareness.md), [names.md](names.md) |
| Firmware version | `Inner::firmware_versions`, kept for the run: [firmware.md](firmware.md) |

**No primary pod.** Each pod advertises as itself and calls itself primary
([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#which-pod-sent-it)),
so the advertisement's flag says only which pod was heard last. In a snapshot, every
advertisement from the other pod of a pair on AAP broadcast a snapshot that differed in
nothing else (#152).

**AAP state goes with the link.** The link holds its reading, mode, settings, name and ear
state (`Link`). Both `open_aap` and `disconnect_aap` broadcast, and disconnect drops all
of the pair's AAP state but the firmware version: kept, a stale reading would go on
looking exact. A mode kept apart from the link once showed the last session's mode after
a reconnect (1fa1632), though it may have been changed from a phone meanwhile.

## Snapshots

`Coordinator::subscribe` returns a `watch::Receiver`: one latest-wins value that every
consumer sees.

- **No consumer competes for a snapshot.** The window, the tray, attachment and the
  notifier each see the same latest one.
- **A slow consumer skips** the snapshots it missed instead of queueing them. Queues per
  consumer, unbounded, grew without limit under an advertisement flood.
- **Sending never waits for a consumer** to take the snapshot, so a slow one stalls no
  read loop. Only an open `borrow()` holds a send up (below).
- **Marked changed** as it is handed out, so the current state arrives at once. Otherwise
  the window came up blank until the first advertisement.

**Never hold `borrow()` across an await.** The borrow is a read lock, and every broadcast
waits for it while holding `inner`'s write lock. A borrow held across an await stalls the
whole coordinator; one held while awaiting the coordinator deadlocks. Clone what is
needed out of it first.

**Sent under the lock it was changed under** (#88). `broadcast(&inner)` takes the state
while the write guard is live, never a snapshot built earlier. Sent after the guard was
dropped, an update in between could send a newer snapshot first, and the older one landed
last and stayed on show. `send_replace` never awaits, so holding `inner` across it is
safe. Every change to `Inner` that changes what a snapshot shows broadcasts.

The window draws at most once per `RENDER_INTERVAL`: [ui.md](ui.md#updates-come-from-snapshots).

## Events

Two consumers need every change, not the latest state: the volume needs every speech
level, and the media pause every change of a pod's place. A short stretch of speech's
`01` could be gone from the watch before a consumer looked. So these go out as events,
in order, on a channel per consumer:

| Events | Read by | Document |
|---|---|---|
| `SpeechEvent` (`Coordinator::speech_events`): each level, and the link coming up and going | `volume::Volume` | [volume.md](volume.md), [conversation-awareness.md](conversation-awareness.md) |
| `EarEvent` (`Coordinator::ear_events`): each pod's place, and the link coming up and going | `media::Media` | [media-pause.md](media-pause.md), [in-ear.md](in-ear.md) |

- **Between the link's coming up and its going.** The consumers of each kind are a std
  mutex beside `inner`. It is taken under `inner`'s write lock to tell an event, so a
  link's events come between its `LinkUp` and its `LinkGone`, and alone to register a
  consumer. It is never held across an await.
- **Before the snapshot that shows them.** An ear event is told under the write lock, ahead
  of the broadcast of the same change.

## The BLE cache

BLE is cached, AAP never is, but for the firmware version.

- **The last advertisement per pair** (`Inner::ble`), the connected pair's too, kept
  underneath its link. When the link goes, the pair falls back to it at once, rather than
  going blank until it next advertises - which, with the lid closed, can be a long wait.
- **Kept for `BLE_CACHE_TTL`** (#169), a day: a pair in a closed case says nothing, and
  its last levels are still a hint of how charged it was. Each reading carries `last_seen`
  for the window.
- **Pruned on a clock.** `expiry_task` prunes each minute: with no pair in range, no
  advertisement ever arrives to trigger a prune.
- **Bounded by the keys.** Only identified advertisements are kept, keyed by real MAC, so
  the map holds at most one entry per stored key: [identification.md](identification.md).
- **Kept across a relaunch** (#170): [readings.md](readings.md).

### The wall clock

The cache's ages are on the wall clock (`SystemTime`, `podstate::age`), never `Instant`.

- `Instant` is `CLOCK_MONOTONIC`, which stands still while the machine sleeps. A reading
  from before a night's suspend read "30 min ago" the next morning, and expired hours
  late.
- A time kept across a relaunch has to mean something to the next run.
- **A reading heard after now** - the clock set back - has no age. It goes at the next
  prune and on load, and is never fresh for a notification.

Every other `Instant` - AAP, ear, noise, volume, media - stays one.

## The lid

How the pods report the lid, and which pod's advertisement carries it:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-6-lid-state).

The parser reads the lid from byte 6 bit 3 only while status bit 6 or bit 2 puts a pod
in the case, and reports no lid otherwise. It does not read byte 6's high nibble.

**Both pods advertise, so one advertisement never wholly replaces the lid.** Replaced
wholesale, the lid blinked between open and unknown several times a second with one pod
worn.

| The advertisement | The lid |
|---|---|
| Reports a lid | Taken |
| Reports none, status bit 4 set: its sender is out of the case, the other pod in it | The cached lid is kept |
| Reports none, bit 4 clear: no pod in the case | No lid |

The parser reports bit 4 as `one_in_case` and stays stateless;
`Coordinator::handle_advertisement` does the keeping.

A closed lid stays on show while the app runs, until the pods advertise again, for up to
`BLE_CACHE_TTL`. A relaunch does not keep it: [readings.md](readings.md#what-is-kept).

## Hardware checks

None open. Two pairs on AAP at once ran on hardware (a Pro 3 and a Pro 2 (USB-C),
2026-10-26, PR #121). The snapshot order (#88) and the advertisement flood have run in
the pipeline tests only.
