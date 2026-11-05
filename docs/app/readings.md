# Readings

What LinuxPods keeps of each pair's last BLE reading across a relaunch, and why (#170).
A pair put away in its closed case says nothing until it is opened again, and its last
levels are still a hint of how charged it was. The file is `readings`; the coordinator
loads and saves it.

## What is kept

`readings.json`, beside the keys (`~/.local/share/linuxpods/`), mode 0600, written to a
temporary file and renamed over the old one, as the keystore is
([keystore.md](keystore.md)).

| Kept | Not kept |
|---|---|
| The levels, the charging flags, the model and color, and when the advertisement was heard | In-ear, the lid, the connection state: stale by the time anything loads them. Nor the advertisement itself. |

So a closed lid stays on show while the app runs ([state.md](state.md#the-lid)), and not
across a relaunch, as the lid may have been opened meanwhile.

The heard time is on the wall clock, as the next run has to tell how old it is:
[state.md](state.md#the-wall-clock).

The file sits in the keystore's directory, so a test's or the fake world's keystore keeps
it out of the user's.

## Loading

At startup, a reading loads only for a pair with keys, heard less than `BLE_CACHE_TTL` ago
and not after now.

- **A file that does not parse is set aside,** and the app starts with no readings. That
  costs only what it would have shown until the pairs are heard again.
- **A level above 100 loads as unknown,** as the parser reads one
  ([battery.md](battery.md)).

**A loaded reading is restored** (`PodState::restored`) until the pair is heard again this
run. It is shown, with its age, but left out of GNOME Settings by
`attachment::exported_levels`: `Battery1` would show yesterday's level as now. It is left
out by that flag, never by a freshness window there, which would withdraw the battery and
export it again (#62): [battery.md](battery.md#gnome-settings).

Checked on a Pro 3, 2026-11-05, at `da79836`: relaunched with the pair in its closed
case, the window showed its last levels with their age, and GNOME Settings showed no
battery for it until it connected again.

## Saving

**Never per advertisement:** a pair in range is heard several times a second.

- **Each minute,** `expiry_task` calls `Coordinator::save_readings`, which writes only
  when `readings::due` says so: a pair has come or gone, a level, a charging flag or the
  model has changed, or a heard time has moved by `readings::HEARD_REFRESH`. The cost of
  that choice: after a crash or a power cut, a relaunch shows a reading up to
  `HEARD_REFRESH` older than it was.
- **Once more at the quit,** from `main.rs`, with every heard time as it is. Waited for
  without a bound: a small file on the local disk, as the keys are, and dropping the
  runtime waits for the blocking pool anyway.

**Never left half done.** The save copies the readings out under `inner`'s read lock, and
writes on the blocking pool with its owned lock moved in. Aborted part way, it runs on, and
no other save starts meanwhile. A save that fails is tried again at the next call.

**The readings lock is taken before `inner`, never under it,** as the keystore's is. Each
save holds its lock across the write, so that saves never run side by side, and takes
`inner`'s read lock under it to copy the readings out. A path that held `inner` and then
took either lock would wait for a save that waits for `inner`: a deadlock.

## Hardware checks

None open.
