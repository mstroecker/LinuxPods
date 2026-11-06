# BlueZ

What bluetoothd does that a client has to know: discovery, adapters and restarts,
starting bluetoothd, battery providers, the A2DP transport's volume, and a device's name
and alias. Read from BlueZ 5.87's source and documentation, and checked on hardware
where a run below says so; what was only read, not run, says so.

The app talks to bluetoothd in `bluez`, `ble::scanner`, `attachment` and `volume`; what
it does with these facts: [app/bluez.md](../../app/bluez.md),
[app/volume.md](../../app/volume.md), [app/names.md](../../app/names.md),
[app/in-ear.md](../../app/in-ear.md).

## Discovery

**`StartDiscovery` answers `org.bluez.Error.InProgress` in two cases** (BlueZ 5.87,
`src/adapter.c`, #123):

| Case | Where | The client afterwards |
|---|---|---|
| It has discovery on this adapter already | `start_discovery` | Still discovers. |
| The kernel refused the start as busy | `discovery_complete` | Is dropped as a discovery client (`start_discovery_complete` calls `discovery_remove`). |

So `InProgress` alone does not say whether the client discovers. A `StopDiscovery` from
a client with no discovery is refused with "No discovery started", and changes nothing
(`stop_discovery` in BlueZ 5.87's `src/adapter.c`, #123).

**The kernel stays busy for seconds after `kill -9 bluetoothd`.** Discovery belongs to
the controller, not to bluetoothd's management socket, so the dead daemon's discovery
runs on: `Device Found` events came until 7 s after the kill (#123, run below). A
graceful `systemctl restart bluetooth` does not leave it: bluetoothd stops discovery as
it shuts down (#123).

**When the last client stops discovering, bluetoothd removes the pods' devices**, as it
does every temporary device that is not connectable and not connected
(`discovery_cleanup` in BlueZ 5.87's `src/adapter.c`, #153). The pods' advertisements
are not connectable. So:

- **With nobody else discovering**, a discovery start hears each pod at once, in the
  `InterfacesAdded` that announces its device (Pro 3, 2026-11-02, #153).
- **While another client discovers** - GNOME Settings' Bluetooth panel, say - the
  devices stay. A pod is then heard only once its payload or address changes (#153).

**An address heard for the first time comes in `InterfacesAdded`,** as bluetoothd makes
a device for it; **each advertisement after that sets `RSSI`** (`btd_adapter_device_found`
in BlueZ 5.87's `src/adapter.c`). Whether that is signaled in `PropertiesChanged` depends
on the discovery:

| Discovery | `RSSI` signaled |
|---|---|
| A discovery filter in force | At every advertisement (`device_set_rssi_with_delta(dev, rssi, 0)`) |
| No filter | Only for a change of 8 dBm or more (`RSSI_THRESHOLD`, `src/device.c`) |

A filter is in force while a client discovering on the adapter has set one
(`SetDiscoveryFilter`) that asks for more than the adapter's own scan type
(`merge_discovery_filters` and `start_discovery_complete`, `src/adapter.c`). Read, not
run on its own; an LE scan with the pods idle saw `RSSI` alone about 4.5 times a second
per pod ([Runs](#runs)).

Why few advertisements reach the app while a pair is connected:
[app/in-ear.md](../../app/in-ear.md#why-aap-and-not-the-advertisement).

## Adapters and restarts

What a client holds on BlueZ - its discovery, its battery providers - is BlueZ's to end:

| Event | What ends | How a client hears of it |
|---|---|---|
| An adapter powers off (GNOME's switch, `rfkill`) | Every client's discovery on it (`adapter_stop` in BlueZ 5.87's `src/adapter.c`, #50; seen 2026-10-26) | `Adapter1.Powered` |
| An adapter goes | Its battery providers (f968d20, #50; not tried on hardware) | `InterfacesRemoved` |
| bluetoothd restarts | Every client's discovery and every battery provider (#50; seen 2026-10-26) | Only `NameOwnerChanged` for `org.bluez` |

- **A restart says nothing else.** The match rules live in the bus daemon and survive
  it, so signals keep arriving from the new bluetoothd and nothing in them looks wrong
  (#50).
- **A restart can show as two owner changes.** Under systemd's bus activation, a read
  after the first can already be answered by the new bluetoothd (f968d20, #50).
- **`systemctl restart bluetooth` powers the adapter off**, and the pairs' connections
  go with their ACL links (2026-10-26, PR #121).
- **`kill -9` takes `Device1.Connected` with bluetoothd, but not the ACL link.** The new
  bluetoothd reported the pairs connected again 0.4 s later (2026-10-26, #123).
- **An s2idle suspend left the adapter powered.** `Discovering` stayed true through 54 s
  of suspend, and no power change was seen (2026-10-26, PR #121, one machine).

Why a signal's sender has to be checked: [zbus.md](zbus.md#senders).

## Starting bluetoothd

**A method call starts bluetoothd, unless it carries `NoAutoStart`.** Under systemd,
`org.bluez` is D-Bus activated: a call to it while bluetoothd is off the bus starts it
again, undoing a `systemctl stop` (2e69ed8, #49; 8d6938e, #132). After `kill -9`,
bluetoothd came back by bus activation within about 300 ms (2026-10-26, PR #121).

## Battery providers

- **Read at registration.** bluetoothd reads a provider's root with `GetManagedObjects`
  as it takes the registration (`battery_provider_new` in BlueZ 5.87's `src/battery.c`,
  through a `GDBusClient`). The call goes with the default timeout (`-1` in
  `gdbus/client.c`), which libdbus takes as 25 s (`_DBUS_DEFAULT_TIMEOUT_VALUE`,
  dbus 1.16.2 `dbus/dbus-connection-internal.h`). bluetoothd takes `InterfacesAdded`
  meanwhile: the client watches for it from its creation (`g_dbus_client_new_full`,
  BlueZ 5.87's `gdbus/client.c`; 52e5c5c, #125). Read, not run against bluetoothd.
- **Keyed on the root path alone.** A root already registered with an adapter's
  manager is refused with `org.bluez.Error.AlreadyExists`, whichever connection asks:
  the check compares the path, not the owner (`register_battery_provider` and
  `match_provider_path` in BlueZ 5.87's `src/battery.c`). So a second client that
  registers the same root on that adapter is refused, and its batteries are never read
  (f968d20, #50). Only `UnregisterBatteryProvider` checks the owner.
- **One adapter's batteries only.** Each adapter's `BatteryProviderManager1` keeps its
  own providers and takes a battery only for a device on its own adapter, the one the
  device's `Device1.Adapter` names. A battery for a device on another adapter is ignored,
  with the warning "Ignoring non-existent device path for battery"
  (`provided_battery_added_cb` in BlueZ 5.87's `src/battery.c`; c2633b7, #51).

What the desktop does with a provider's battery: [desktop.md](desktop.md#upower).

## MediaTransport1.Volume

`org.bluez.MediaTransport1.Volume` (`q`, 0-127, writable, signaled) is the AVRCP absolute
volume of the pair's A2DP transport. It goes to the pods themselves (#150).

- **No owner check.** BlueZ 5.87's documentation (`org.bluez.MediaTransport(5)`) says
  `Volume` is writable only by the client that acquired the transport. bluetoothd checks
  no owner for it (`profiles/audio/transport.c`): `busctl`, which was not the owner, set
  it (Pro 3, 2026-11-01, #150; the run: [desktop.md](desktop.md#the-volume)).
- **A path of its own per A2DP setup.** BlueZ makes a transport for each A2DP setup, at
  `<device>/sep1/fd<n>`, `n` the first number free from the count of all bluetoothd's
  transports (`media_transport_create` in BlueZ 5.87's `profiles/audio/transport.c`).
- **A `Set` changes nothing at once.** In BlueZ 5.87's source, a host-as-source
  transport's setter (`transport_a2dp_src_set_volume` in `profiles/audio/transport.c`):
  - does nothing for the volume the transport holds already;
  - otherwise sends the volume to the pods over AVRCP (`avrcp_set_volume`), and changes
    nothing.

  The property changes once the pods answer (`avrcp_handle_set_volume` in
  `profiles/audio/avrcp.c`), to the volume they report, which may be rounded. That is a
  reading of the source: how long the pods take, and whether they round, is untried
  here.
- **Kept on the device.** bluetoothd keeps the volume on the device and starts its next
  transport at it (`btd_device_set_volume` in BlueZ 5.87's `profiles/audio/transport.c`).

What the app does with each of these: [app/volume.md](../../app/volume.md).

## Device1.Name and Device1.Alias

The pods' own name, which BlueZ reads as `Device1.Name`:
[protocol/aap/rename.md](../aap/rename.md#two-names).

`Device1.Alias` is this computer's label for the pair.

- **GNOME Settings' rename sets it** (#40). It never reaches the pods
  (#40, PR #62).
- **Until one is set, the alias is the name**, so it follows the pods' name. Once set, it
  stays whatever the pods are called (BlueZ 5.87, `org.bluez.Device(5)`;
  `dev_property_get_alias` in `src/device.c`).
- **Only an empty string clears it** (BlueZ 5.87, `org.bluez.Device(5)`). Any other
  string is kept as the alias, the pods' current name too (`set_alias` in BlueZ 5.87's
  `src/device.c`). So typing the old name back in GNOME's rename sets an alias rather
  than clearing it - assuming GNOME sends the name as typed (#40), a guess.

## Runs

**Discovery start: Pro 3 in the open case, not connected, 2026-11-02, ce9ed40 (#153).**

- With nothing else discovering, the app restarted several times. Each time the battery
  showed 36 ms after discovery started. Once the app had quit, bluetoothd had removed
  both pods' devices.
- With GNOME Settings' Bluetooth panel open, the pods were heard 2.1 s and 4.1 s after
  discovery started, from the addresses they had before.
- A 40 s LE scan (`bluetoothctl scan le`, D-Bus signals recorded) with the pods idle in
  the open case: the two pods' devices got 178 and 179 `PropertiesChanged` with `RSSI`
  alone (about 4.5 a second), and no `ManufacturerData` change.

**Adapters and restarts: one USB adapter, BlueZ 5.87, a Pro 3 and a Pro 2 (USB-C) both
connected over AAP, 2026-10-26, abc0e13, `RUST_LOG=linuxpods=debug` (PR #121).**

| Step | What bluetoothd did |
|---|---|
| Bluetooth off and on in GNOME Settings | Discovery ended at the power-off; both pairs reconnected after the power-on. |
| `rfkill block` / `unblock` | The same. The adapter kept its name. |
| `systemctl restart bluetooth` | It powered the adapter off; the AAP links went with the ACL links. |
| `kill -9 $(pidof bluetoothd)` | Back by bus activation within about 300 ms; its discovery start was refused (next run). |
| Suspend and resume (54 s, s2idle) | `Discovering` true throughout, no power change. |

**`kill -9 bluetoothd`: the same machine and day, with `btmon` and `dbus-monitor` (#123),
reproduced twice.** Times as in #123's capture; the kill came about 2.2 s in.

| Time | What happened |
|---|---|
| +2.8 s | The new bluetoothd sent `MGMT Start Service Discovery` and got `Status: Busy (0x0a)`. |
| +2.9 s | It answered the client's `StartDiscovery` with `org.bluez.Error.InProgress` ("Operation already in progress"), 123 ms later, and dropped it as a discovery client. |
| until +9.2 s | `Device Found` events from the dead daemon's discovery, then `Discovery: Disabled`. Nothing scanned after that. |

## Open questions

- Do the pods answer a set of `Volume` with another volume than the one set, by how
  much, and how long do they take? No run of its own: the app's debug log (`RUST_LOG=linuxpods::volume=debug`) of a
  stretch of speech shows each volume set beside what the transport read then, and
  `examples/conversation_awareness.rs` with `--duck` what it read back after each set.
- An adapter that goes - a USB dongle unplugged - taking its battery providers: not
  tried on hardware (PR #121).
- `rfkill` re-enumerating the adapter to a new `hciN`: not tried; the internal adapter
  does not (PR #121).
- bluetoothd waiting 25 s for a provider's `GetManagedObjects` reply: read, not seen
  (#125).
- Does GNOME's rename send the name as typed, so that typing the old name back sets an
  alias? A guess, not tried.
