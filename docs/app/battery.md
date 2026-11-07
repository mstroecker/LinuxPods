# Battery

How Cymba reads a pair's battery levels, what counts as unknown, and where a level
goes: the window, the tray and GNOME Settings. The packet is the pods':
[protocol/aap/battery.md](../protocol/aap/battery.md); the advertisement's levels:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md).

## Unknown is `None`

A level is an `Option<u8>`, never a sentinel: no 0, 255 or -1 standing for unknown.

| The reading | The level |
|---|---|
| Above 100, over AAP or BLE | Unknown |
| An AAP part with status Disconnected (`0x04`), whatever its level byte | Unknown |
| An AAP part with a status the parser does not know | The level counts |

**Disconnected is no 0%** (#108). The level byte beside status `0x04` is a placeholder,
and it comes as `0x00` as well as `0xFF`
([protocol/aap/battery.md](../protocol/aap/battery.md#disconnected-is-no-0)). Read as a
level, the `0x00` showed the case at 0% while both pods were worn.

**Entries are read by their component code,** not by their place: the pods list the
primary pod first, and the order changes at a role switch
([protocol/aap/battery.md](../protocol/aap/battery.md#order-of-the-entries)).

## Where a reading comes from

- **Over AAP,** from the link's battery packets, from the first one on:
  [state.md](state.md#per-pair).
- **Over BLE,** from the decrypted advertisement of an identified pair; the cleartext
  levels are never shown: [identification.md](identification.md#batteries-in-the-clear-and-encrypted).
- **From the last run,** until the pair is heard again: [readings.md](readings.md).

## Where it goes

| Where | What |
|---|---|
| The window | Each pod and the case, with its charging state, and the age of a BLE reading |
| The tray | The lower of the two pods (`PodState::lowest_earbud`) |
| GNOME Settings | The lower of the two pods, below |
| A low battery notification | Each part at or below `notify::LOW`: [notifications.md](notifications.md) |

### GNOME Settings

GNOME Settings reads a pair's battery from bluetoothd, which takes it from a battery
provider (`org.bluez.BatteryProvider1`, [bluez.md](bluez.md#battery-providers)). Attachment
exports one battery per attached pair with a reading (`attachment::exported_levels`).

- **No reading, no battery.** `Battery1` has no "unknown". An object exported ahead of the
  first reading sat at 0%, a real level, so GNOME Settings showed a flat battery for the
  whole connection if no reading came through. Absence is the only honest way to say
  "unknown".
- **A restored reading is left out** until the pair is heard again this run: `Battery1`
  would show yesterday's level as now ([readings.md](readings.md#loading)).
- **Never by a freshness window.** A battery that came and went with a window would be
  withdrawn and exported again, as on every reconnect before #62.
- **A detached pair's battery is withdrawn** as it goes.

GNOME never warns of a low Bluetooth battery
([protocol/linux/desktop.md](../protocol/linux/desktop.md#upower)), so the app warns
itself: [notifications.md](notifications.md).

## Hardware checks

None open. The case read as unknown with both pods worn is pinned by #108's capture (Pro 3)
in the parser's tests.
