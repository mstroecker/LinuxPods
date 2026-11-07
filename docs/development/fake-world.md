# The fake world

What the fake AirPods, the fake BlueZ and the fake media player do, and where they
depart from the hardware. The fakes follow the #71 captures of an AirPods Pro 3 and the
runs since, for every model: a Pro 2 (USB-C) counts as behaving the same until a
capture shows otherwise. Where the hardware's behavior is not recorded, the fake makes
a choice, listed in [Open questions](#open-questions).

How to start the fake world, change it and drive the app's UI:
[.claude/skills/fake-world/SKILL.md](../../.claude/skills/fake-world/SKILL.md). Every
method and every behavior in detail is in the module doc comments this page links to.

**Byte offsets** count from 0, from a packet's first byte.

## The parts

| Part | What it is | Source |
|---|---|---|
| The fake AirPods | One pair's state, and the bytes it sends over BLE and AAP. Pure: state in, bytes out, the time an argument. Every packet the app reads comes from the builders beside its parsers, so the fake cannot encode those otherwise than the app reads them. The packets the app does not read (`0x0C`, `0x0E`, `0x2E`, `0x2B`, the startup dump's stand-ins) are built in `pods.rs` alone. | [`src/harness/pods.rs`](../../src/harness/pods.rs) |
| The fake BlueZ | `org.bluez` on a private bus: adapters, devices, discovery, advertisement monitors, battery providers, A2DP transports | [`src/harness/bluez.rs`](../../src/harness/bluez.rs) |
| The world | The pairs, the fake BlueZ and each pair's AAP socket running together, changed in the same process or over D-Bus (`FakeWorld1`) | [`src/harness/world.rs`](../../src/harness/world.rs) |
| The fake media player | An MPRIS player on the app's private session bus, `org.mpris.MediaPlayer2.fakeplayer` under `fakepods` | [`src/harness/player.rs`](../../src/harness/player.rs) |
| `fakepods` | The process `make fake-world` runs: the world on a private bus | [`src/bin/fakepods.rs`](../../src/bin/fakepods.rs) |

The pipeline tests drive the same world from the test process.

## Where it departs from the hardware

| What | The fake | The hardware |
|---|---|---|
| AAP transport | A Unix seqpacket socket per pair (`aap::client::unix_socket_path`), in place of L2CAP. The pending-connect handling of #136 never runs, and no connect pages a pair. | L2CAP on PSM 4097: [protocol/linux/l2cap.md](../protocol/linux/l2cap.md) |
| Address rotation | Every `ADDRESS_ROTATION`, far faster on purpose, so tests meet rotation. The left pod's clock runs half a rotation off the right's. | Every 4-8 minutes, each pod on its own clock (#71): [protocol/ble/addresses.md](../protocol/ble/addresses.md#rotation) |
| Advertising rate | One advertisement per `ADVERTISING_INTERVAL` from each pod that advertises | Faster, with bursts: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#update-frequency) |
| Payload changes | Only when the state changes | Also again about 5 s, 30 s and 60 s after a change with nothing else changing (Pro 3 and Pro 2 (USB-C), 987ffe3), and once about 2 s after discovery starts (Pro 3, #153): [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#update-frequency) |
| Handshake answer | At once, unless `handshake_delay_ms` is set | Later: [protocol/aap/session.md](../protocol/aap/session.md#step-2-send-handshake) |
| Battery after the notification request | At once, unless `report_delay_ms` is set | Later: [protocol/aap/session.md](../protocol/aap/session.md#initial-state-report) |
| Startup dump | `DUMP_DELAY` after the first `0x0C`, unless `dump_delay_ms` is set. Any delay keeps the order: battery and ears first, the mode later. | Later: [protocol/aap/session.md](../protocol/aap/session.md#initial-state-report) |
| Closed case | A pod in the case is in the closed case the moment the lid closes | Seconds later: [protocol/aap/ear.md](../protocol/aap/ear.md#ear-report-0x06) |
| Role switch | The `0x06` in the new order and the battery, then two `0x08`s, `PRIMARY_AFTER_EAR` and `SECOND_PRIMARY` apart | The battery comes between the two `0x08`s: [protocol/aap/ear.md](../protocol/aap/ear.md#the-role-switch) |
| Device information (`0x1D`) | Laid out as the hardware's, with fields 1-9 made up | [protocol/aap/device-info.md](../protocol/aap/device-info.md#layout) |
| bluetoothd restart | A crash: `NameOwnerChanged` twice, nothing announced again. A test passing on a restart proves the `NameOwnerChanged` path only. | A new bluetoothd also announces every object again with `InterfacesAdded`; a graceful stop first powers the adapters off and releases every monitor: [protocol/linux/bluez.md](../protocol/linux/bluez.md#adapters-and-restarts) |
| Adapter powered off | `Powered` first, then the adapter stopped as rfkill stops it | bluetoothd removes the devices first, and `Powered` follows (BlueZ 5.87, `src/adapter.c`) |
| Advertisement monitors | Registration, `Activate`, `DeviceFound`, `DeviceLost` and `Release` as BlueZ 5.87's source has them, read, not run. The RSSI thresholds and sampling period are not read: a device is lost `MONITOR_LOST_TIMEOUT` after its last advertisement, whatever they are. A power-off forgets found devices without `DeviceLost`, where bluetoothd reports those whose lost timer runs lost at once. A monitor whose pattern has an AD type bluetoothd does not know is taken, where bluetoothd releases it (`bt_ad_pattern_new`) | [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising) |
| `RSSI` through a monitor | Signaled at every advertisement, as under discovery. The app drops `RSSI` alone | Far less often with no discovery filter in force (#189): [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising) |
| Experimental interfaces turned on | At once (`SetExperimental`); follow it with `RestartBluez` | Read only as bluetoothd starts: [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising) |
| Experimental interfaces off | A stand-in `AdvertisementMonitorManager1` on each adapter refuses every call with `UnknownMethod`, where zbus would answer `UnknownInterface` for one not there ([protocol/linux/zbus.md](../protocol/linux/zbus.md#calls-to-an-interface-not-served)). It shows among the adapter's interfaces. | No monitor manager on the adapter; `RegisterMonitor` refused with `UnknownMethod`: [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising) |
| A monitor released (`ReleaseMonitors`) | Its registration forgotten too. The app asks again only under a new registration token, at a new root, so keeping it, as bluetoothd does, would change nothing it sees | The registration kept (BlueZ 5.87, `src/adv_monitor.c`, read, not run) |
| No battery API (`drop_battery_api`) | Registering a provider is refused with `UnknownInterface`, as zbus answers for an interface not served ([protocol/linux/zbus.md](../protocol/linux/zbus.md#calls-to-an-interface-not-served)). The app does not tell the errors apart. | `UnknownMethod`, as libdbus answers a call nothing handles: [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising) |
| Battery provider read | No signal from a provider is taken until its `GetManagedObjects` is answered, so a provider that leaves the read unanswered gets no battery (#125) | bluetoothd takes `InterfacesAdded` regardless (BlueZ 5.87, `gdbus/client.c`): [protocol/linux/bluez.md](../protocol/linux/bluez.md#battery-providers) |
| A2DP transport paths | `<device>/sep1/fd<n>`, `n` new to the device with every transport | `n` the first free from the count of all bluetoothd's transports (BlueZ 5.87, `profiles/audio/transport.c`). Nothing depends on the number. |
| A2DP `Volume` set | Every set that succeeds brings a `PropertiesChanged` as it returns, with the volume the transport holds by then ([protocol/linux/zbus.md](../protocol/linux/zbus.md#property-setters)): one held already, or one not answered yet, brings one that changes nothing. Nothing in the app watches the transport's signals: it reads `Volume` afresh. | A set of the volume held does nothing, and any other changes `Volume` only once the pods answer: [protocol/linux/bluez.md](../protocol/linux/bluez.md#mediatransport1volume) |
| Audio server | None: the world's methods do what PipeWire, or the user, would | PipeWire sets A2DP up |

The defaults are a Pro 3's. `SetQuirk` switches the fake pods to AirPods 4's answers to a
mode change, or to a handshake left unanswered (seen once, with the Flatpak build; when, and on which model, is not recorded), or a
connection turned away (`Quirks` in [`src/harness/pods.rs`](../../src/harness/pods.rs)).

## The fake AirPods

### The session

The fake pods answer what a link sends as it comes up in the order of capture 4 of #71
([protocol/aap/session.md](../protocol/aap/session.md#initial-state-report)):

- **The handshake:** its answer, all 18 bytes, and `0x2B`, the hosts the pair knows,
  unasked. Until the answer is out, everything else is dropped, as the hardware drops it.
- **The features packet (`0x4D`):** no answer.
- **The notification request (`0x0F`):** the battery, `0x08`, `0x0C` naming the pair
  itself, `0x0E` and `0x2E` naming this host, `0x06`, and `0x2E` again. After the delay
  of the startup dump, a second `0x0C` names this host, and the dump follows: the mode
  among some twenty settings.
- **The key request (`0x30`):** the key packet, IRK then ENC_KEY. `Repair` gives the
  pair new keys, sent at the next key request.

Where a packet names a host, the fake gives this host `THIS_HOST`, an address from the
range set aside for documentation. The app reads none of them.

### Batteries

The batteries come primary pod first, as the hardware lists the bud AAP reached first
(#71, #142). A part that is away goes out as status Disconnected (`0x04`) at level 0, as
the hardware sends it (#108).

### Ear detection

What the hardware sends: [protocol/aap/ear.md](../protocol/aap/ear.md).

After the startup reports, a `0x06` comes on every change of a pod's place.

- **On a lid change:** the `0x08` with byte 8 (`SS`, whether the secondary pod can be
  reached) changed, then the `0x06`, then the battery.
- **On a role switch:** see [the table above](#where-it-departs-from-the-hardware).
- **Both pods in the closed case:** the pods drop the link. The world disconnects the
  pair in the fake BlueZ, then closes its AAP connection, as a disconnect does.
- `SetInEar` and `SetInCase` of `FakeWorld1` move a pod over D-Bus, as `Pods::set_in_ear`
  and `Pods::set_in_case` do in the pipeline tests.

### Noise control

What the hardware sends: [protocol/aap/noise-control.md](../protocol/aap/noise-control.md).

| Change | The fake pods' answer, by default |
|---|---|
| Any mode | An echo, `ECHO_DELAY` later, as a Pro 3 echoed 1.0-1.1 s after each change (#71) |
| Off | First a report of the mode it replaced, `STALE_REPORT` later, as a Pro 3 sent at 0.33 s (#113) |
| Off, not allowed by `0x34` first | No change. With Conversation Awareness off, nothing on the wire; with it on, level `08` at once, then a report of the mode it stayed in and `09`, `echo_delay` later, as a Pro 3 sent them (#150). The hardware also plays an error chime. |
| Any other, Conversation Awareness on | Level `08` at once and `09` after the echo, as a Pro 3 sent them (#150) |

### Conversation Awareness

What the hardware sends: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md).

`Speak` has the user speak for a while. With Conversation Awareness off, the pods do
nothing. With it on, they send the levels and switch to Transparency and back at the
offsets of the first stretch of the #150 run (the `SPEECH_*` constants): a stretch too
short for a step leaves it out. `SetConversationAwareness` changes the setting as a
phone would: nothing reports it over AAP, and the next connection's startup dump
carries it.

### Device information

The packet's layout: [protocol/aap/device-info.md](../protocol/aap/device-info.md#layout).

The fake pods send `0x1D` with the pair's name in field 0, fields 1-9 made up, and field
10 `MARKETING_VERSION` - the hardware's value from 2026-11-02 (#161) - until a test
changes it (`Pods::set_marketing_version`). So the fake world shows the Firmware row.

### Rename

What the hardware does: [protocol/aap/rename.md](../protocol/aap/rename.md).

The fake pods take a rename at once and answer nothing, as a Pro 3 did (#160). A name
over `NAME_KEPT` bytes is cut to its first 127, inside a character if one falls there, as
a Pro 3 cut it. The next connection's `0x1D` carries the name, and the fake BlueZ's
`Name` once the pair connects again. `Name` of `FakeWorld1` reads it back.

### Advertisements

What the hardware sends: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md).

As in the #71 captures:

- **Who advertises:** both pods while either is in the case, only the primary while
  both are out.
- **The closed case:** a pod in it stops `LID_SILENCE` after the lid closes. When the
  lid opens, the pods come back under new addresses, the right pod `RIGHT_RESUMES`
  later, the left `LEFT_RESUMES`.
- **The address:** a resolvable private address that the pair's IRK resolves (#134).
- **The MAC suffix** (plaintext bytes 7-9) is there only while both pods are in the
  case, and `00 00 00` otherwise.
- **Plaintext bytes 12-15** follow from the rest of the payload, as a MIC would, so they
  change with it and only with it.

`Flood` brings advertisers that no key decrypts and no IRK resolves, each from an
address of its own.

## The fake BlueZ

What bluetoothd does that the app depends on:
[protocol/linux/bluez.md](../protocol/linux/bluez.md). The fake serves only what the
app calls: no pairing, no `Device1.Connect`, no agent.

- **Adapters** can be powered off and on, plugged in and out, and bluetoothd restarted,
  at any time (#50). `set_discovery_busy` has an adapter refuse discovery with
  `InProgress`, as after a `kill -9` of bluetoothd (#123).
- **Experimental interfaces** are off, as in a stock bluetoothd, so the app discovers.
  `fakepods --experimental` (`EXPERIMENTAL=1` for `make fake-world`) or `SetExperimental`
  turns them on: every adapter offers an `AdvertisementMonitorManager1`, and the app
  listens through a monitor. `ReleaseMonitors` releases an adapter's monitors.
- **An adapter hears** advertisements while it is powered and a client discovers or has
  a monitor registered there.
- **Advertisements** are signaled as bluetoothd signals them: a new address as
  `InterfacesAdded`, each later advertisement as `PropertiesChanged` with `RSSI`, and
  `ManufacturerData` only when it changed. An address goes `TEMPORARY_TIMEOUT` after it
  went quiet, and all of an adapter's advertisers go when the last client stops
  discovering there (#153). Every adapter that hears advertisements hears every one.
- **Connecting** a pair signals `ServicesResolved` `SERVICES_RESOLVED` after
  `Connected`.
- **Battery providers** are kept per adapter, as BlueZ 5.87's `src/battery.c` keeps
  them. A battery for a device that is not paired on the provider's adapter is ignored.
  `Batteries` of `FakeWorld1` reads what GNOME Settings would show.

### The volume lowered while the user speaks

What the app does: [app/volume.md](../app/volume.md).

- **A2DP:** `SetPlaying` gives a connected pair an A2DP transport, `active`, at the
  pair's own volume; stopped, it goes `idle`.
- **A call** takes A2DP to `idle`, and back after: see [Open questions](#open-questions).
- **`Volume`** is writable by any client, and each client's set is recorded
  (`VolumeSets`). By default a set takes effect at once. `SetVolumeAnswer` has the pair
  answer late and rounded to even, as bluetoothd changes `Volume` only once the pods
  answer (BlueZ 5.87).
- **`SetVolume`** is the user's change, such as GNOME's slider.

## The fake media player

What the app does: [app/media-pause.md](../app/media-pause.md).

It serves as much of MPRIS as the app uses: `PlaybackStatus`, `CanPlay`, `CanPause`,
`CanControl`, and `Play`, `Pause`, `PlayPause` and `Stop`. It starts `Stopped`. What the
user does in the player goes through `FakePlayer1.SetStatus`, and `Calls` lists every
MPRIS method the app called, in order. `fakepods` refuses to put it on the user's own
session bus.

## Open questions

Where the hardware's behavior is not recorded, and the fake's is its own choice:

- **A mode set during speech:** the fake pods go back to it after the stretch.
- **What `0x34` (allow Off) and `0x28` (Conversation Awareness) answer:** nothing.
- **A connection turned away:** taken and closed at once, unanswered (`refuse_aap`).
- **A rename shaped otherwise, or an empty name:** ignored.
- **A lid opened before the pods went quiet:** they go on advertising from the addresses
  they had.
- **A call:** A2DP goes `idle` and comes back, rather than staying or going. That is how
  PipeWire's own HFP backend is assumed to show it, not read in its source nor watched
  (#150).
- **How long the pods take to answer a volume, and whether they round:** untried (#150).
- **A monitor across a power cycle:** kept, and hearing again once the adapter is on
  ([protocol/linux/bluez.md](../protocol/linux/bluez.md#open-questions)).
- **The pods' devices when another client stops discovering while a monitor is
  registered:** removed, as with no monitor, and announced again with `DeviceFound` at
  the next advertisement
  ([protocol/linux/bluez.md](../protocol/linux/bluez.md#open-questions)).
