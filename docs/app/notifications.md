# Low battery notifications

When Cymba warns of a low battery, and why it has to (#126). GNOME never warns of a
low Bluetooth battery: UPower gives `Battery1` batteries no warning level
([protocol/linux/desktop.md](../protocol/linux/desktop.md#upower)). So the app warns
itself.

## What is warned about

`notify::Alerts` works it out on the tokio side, from the snapshot stream and the time;
`ui::notifications` sends it from the main context as a `GNotification`.

**One notification per pair,** with an ID of its own, naming its low parts: "Left 9%,
Right 12%". It replaces itself as the parts warned about change, and is withdrawn once
none is left.

**The thresholds are UPower's defaults:** `notify::LOW` and `notify::CRITICAL`
([protocol/linux/desktop.md](../protocol/linux/desktop.md#upower)). A level at a threshold
counts as below it, as in UPower.

**Once per crossing, never per snapshot.**

| A part | The notification |
|---|---|
| First at or below `LOW`, or `CRITICAL` | Warned about |
| Back above the threshold, not above `notify::REARM_ABOVE` | Unchanged: the worst band warned about stands |
| Above `REARM_ABOVE`, or charging | Armed again; dropped from the notification |
| With no level - the case, lid shut with no pod in it | Neither warns nor arms |

So a reading flipping across a threshold sends nothing. Armed again as soon as it was
above `LOW`, a part read at 20% and 21% by turns would be warned about, withdrawn and
warned about again at every turn. Its readings come from more than one source,
which can lag each other by a step: both pods advertise, and AAP and the BLE reading it
falls back to can differ. `REARM_ABOVE` is `LOW` and five points, which clears that step
many times over, while a part really charged - more than a few minutes in the case - is
past it at once; and charging arms it anyway.

**Only from a fresh reading:** over AAP always, over BLE only when heard within
`notify::FRESH` (#126, #169). The BLE cache keeps a pair's last reading for `BLE_CACHE_TTL` after
it has gone, and across a relaunch ([state.md](state.md#the-ble-cache)), and that says
nothing about the battery now.

`FRESH` is a minute. A BLE reading comes only when a pod's payload changes or a new
address is heard, or a monitor reports found a pod no signal from bluetoothd has brought since it was
last lost
([bluez.md](bluez.md#listening-for-the-pods)), and an idle pair can keep one payload for
minutes
([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#update-frequency)):
a reading older than `FRESH` may be from a pair still in range, and is not warned about.
The window
draws the same line: under `ui::RECENT` is "Last seen just now". A reading is judged by its age when a snapshot comes in, so a
cached one that comes along with another pair's update, or with the expiry clock's, warns
about nothing.

| A reading | |
|---|---|
| Fresh | Warns, arms and withdraws |
| Stale | Does none of these: a pair that went quiet while low keeps its notice |
| Heard after now, the clock set back | Never fresh: [state.md](state.md#the-wall-clock) |
| Gone from the snapshot | Its notice is withdrawn, and what was warned about forgotten |

A pair's reading leaves the snapshot only when its last advertisement expires from the
cache, or its link goes with no advertisement behind it. Never while BlueZ restarts or the
pair is renamed: the cache is the coordinator's, keyed by the real MAC.

The pair is named by `Snapshot::display_name`: [names.md](names.md#two-names).

## The switch

Preferences' Battery notifications switch is the app action `app.battery-notifications`
([ui.md](ui.md#settings)). The delivery loop on the main context reads it, not the
notifier:

- **Off** withdraws what is on show.
- **On** shows what is low as it stands, once each, a notice dismissed before included.
- **Meanwhile** `notify::Alerts` goes on tracking crossings: a crossing taken back while
  the switch was off is not warned about, and a part that went low then is not left out.

## Hardware checks

None open. The UPower behavior behind this was seen on a Pro 3 (2026-10-26, #126).
