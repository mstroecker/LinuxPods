# BlueZ and D-Bus

How LinuxPods works with bluetoothd over D-Bus: finding the adapters and following them,
listening for the pods - an advertisement monitor, or discovery - the battery providers,
the subscriptions that advertisements reach, and which signals and calls it believes. The code is in `bluez`, `ble::scanner` and `attachment`. What
bluetoothd and zbus do, with their sources:
[protocol/linux/bluez.md](../protocol/linux/bluez.md),
[protocol/linux/zbus.md](../protocol/linux/zbus.md).

## Adapters

**Found, never assumed** (#51). `bluez::watch_adapters` reads the adapters from
`GetManagedObjects` and follows them as they power off and on, come and go, and
bluetoothd restarts (#50). With `/org/bluez/hci0` hardcoded, a machine whose adapter was
another heard nothing over BLE.

**What a client holds is BlueZ's to end:** an adapter powering off ends its discovery, one
going drops its battery providers and releases its advertisement monitors (read, not
run), and a restart drops all of them, saying so by `NameOwnerChanged` alone after a
crash; a graceful stop powers the adapters off (seen, PR #121) and releases the monitors
first (read, not run)
([protocol/linux/bluez.md](../protocol/linux/bluez.md#adapters-and-restarts),
[monitors](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
Started once at startup, discovery went with the first power cycle and the provider with
the first restart, with nothing in the log (#50).

So `watch_adapters` turns what it hears into `AdapterSet` tokens, each tied to the
bluetoothd instance it was read from. A consumer starts again when the token changes,
never on a single signal.

## Listening for the pods

**An advertisement monitor first, discovery where there is none** (#189). Discovery that
never ends keeps some devices from connecting: while it runs, the kernel's background
scan is off, and a device that reconnects by directed advertising - a bonded mouse, say -
stays unconnected until it stops. A monitor on the pods' proximity pairing data hears
their payload over the background scan instead, and leaves such a device free to connect
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
But bluetoothd offers monitors only with its experimental interfaces on, which they are
not by default. So `ble::scanner::Scanning` decides per powered adapter:

| On the adapter | The scanner |
|---|---|
| The monitor is registered, or BlueZ answers `AlreadyExists` | Starts no discovery there, and stops its own if it ran |
| `RegisterMonitor` is refused with `UnknownMethod`: the experimental interfaces are off | Discovers there |
| bluetoothd calls the monitor's `Release` | Discovers there |
| The registration fails otherwise | Asks again after `bluez::backoff`, discovering there meanwhile |

**A failed registration discovers meanwhile:** the pods stay heard, and a device that
reconnects by directed advertising waits until the monitor is taken. Without discovery,
the app would hear nothing there for as long as the backoff, up to `bluez::RETRY_MAX`.

The monitor matches `ble::scanner::PROXIMITY_PATTERN` at the start of the manufacturer
data, with `RSSISamplingPeriod` 0 and no RSSI thresholds.

**One monitor root per registration,** named after the adapter and its `AdapterSet`
registration token. So a `Release` says which adapter's monitor went, and of which
registration: one that comes late for a monitor already replaced, as when an adapter is
unplugged and plugged in again, is not the new one's and counts for nothing. The root of
a registration that is gone is taken off the connection.

**The calls the scanner makes for an adapter go to bluetoothd's unique name:** the
registration, the discovery starts and stops, the read of a device found. A `Release` can come before the adapter watch has
heard that the adapter or bluetoothd is going, since bluetoothd releases the monitors of
an adapter that goes, and of every adapter as it stops
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising),
[restarts](../protocol/linux/bluez.md#adapters-and-restarts)).
The discovery start that follows then fails, where a call to `org.bluez` could have
started bluetoothd again after `systemctl stop bluetooth`
([Never starting bluetoothd](#never-starting-bluetoothd)).

**Decided again when the adapters change.** A monitor stands while the adapter's
`AdapterSet` registration token does, as a battery provider does: bluetoothd keeps it
while the adapter is off, and drops it with the adapter or a restart
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
A user who turns the experimental interfaces on restarts bluetoothd, as it reads them
only as it starts
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)),
which brings a new token, and the scanner registers a monitor with no app restart. A refused or released
monitor is asked for again only then too.

**A device found is read from BlueZ.** While another client discovers, bluetoothd keeps
the pods' devices, and a monitor registered then hears of a pod in `DeviceFound` alone,
with no `InterfacesAdded` and no payload change
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
So for a `DeviceFound` of a device no signal from bluetoothd has brought since it was last lost, the
scanner reads the device's properties from bluetoothd and takes its payload as an
advertisement. The read is made by `Scanning::next`, the
scanner's reader, never by a task that drains a subscription
([Subscriptions that advertisements reach](#subscriptions-that-advertisements-reach)).

**Only a device no signal from bluetoothd has brought since it was last lost is read.** bluetoothd
signals every payload change of a device on the scanner's subscription
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)),
so a device whose payload has come there, and that no monitor has reported lost since,
needs no read. A flood of advertisers from a new
address each, every one of them found, would otherwise put a call to bluetoothd between
every advertisement the reader takes, which discovery never did. The drain records each
device whose Apple data a signal carried, with the signal's sender, the last
`ble::scanner::HEARD_DEVICES` of them; one forgotten costs a read again, never a payload.
A device counts as heard only from bluetoothd's unique name
([Which signals count](#which-signals-count)): a signal forged with a pod's payload
would otherwise spare its `DeviceFound` the read, and the pod would go unheard until its
payload changed. bluetoothd sends a new
device's `InterfacesAdded` before its `DeviceFound`
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)),
but the two reach the scanner by two tasks: the reader yields once before it looks, and a
`DeviceFound` taken before the drain has recorded the device still costs a read.

**A device lost is read again when found.** bluetoothd keeps a lost pod's device a while
([protocol/linux/bluez.md](../protocol/linux/bluez.md#runs)), and a pod found again
with its payload unchanged brings no signal. So a `DeviceLost` forgets the device as
heard, and its next `DeviceFound` reads it.

The monitor's calls wait for the reader coalesced: a device found twice is read once. A
loss takes back the device's find waiting before it, so that a find after the loss comes
after it; a device found and lost before the reader took either is not read, as the
monitor heard it no more meanwhile. Nothing else is dropped. They are taken in the order bluetoothd sent them: the monitor is served with
`spawn = false`, as zbus would otherwise run each call in a task of its own, where a
`Release` could overtake the `Activate` before it, and its methods only push into the
queue
([protocol/linux/zbus.md](../protocol/linux/zbus.md#the-object-servers-dispatch)).
Beyond that, `DeviceLost` changes nothing: a reading expires in the coordinator
([state.md](state.md#the-ble-cache)).

**`ble::scanner::ScanControl` is there for the rest of the app,** made once in `main.rs`
and kept across the scanner's restarts. The Preferences hint reads it and asks it for the
pause ([ui.md](ui.md#the-scanning-hint), #189):

- `ScanControl::state` is a watch of `ScanState`: `fallback`, whether a powered adapter
  has no monitor and so has to discover - also while discovery is paused, or its starts
  fail - and `paused_until`, the end of a pause.
- `ScanControl::pause_discovery` stops the app's discovery on every adapter for
  `DISCOVERY_PAUSE`, whatever the table above says, and starts it again after that. A
  device waiting to reconnect connects as soon as discovery stops
  ([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
  Nothing is heard over discovery meanwhile. Another client's discovery, such as GNOME
  Settings' Bluetooth panel, still blocks.

### Discovery

**On every adapter that has no monitor, subscribed first.** An address's first
advertisement comes in the `InterfacesAdded` that announces it. With nobody else
discovering, every pod's is such a first: bluetoothd removes the pods' devices when the
last discovery ends, so a start hears each pod at once (#153;
[protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery)).

**`InProgress` from `StartDiscovery` never counts as started** (#123). bluetoothd sends it
to a client that discovers already, and also when the kernel refuses the start as busy,
dropping the client - as it is for seconds after `kill -9 bluetoothd`
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery)). On `InProgress` the
scanner stops its discovery, which BlueZ refuses harmlessly if there is none, and starts
again; a second refusal waits for the backoff. Taken as started, BLE stayed dead until
the app restarted.

The scanner's discovery filter sets only `Transport`, which leaves BlueZ to drop
duplicates: [in-ear.md](in-ear.md#why-aap-and-not-the-advertisement).

## Battery providers

How GNOME Settings shows a pair's battery: [battery.md](battery.md#gnome-settings).

- **A provider root per adapter, per connection,** named after the connection's unique
  name and registered with that adapter's `BatteryProviderManager1`. bluetoothd keys
  providers on the root alone
  ([protocol/linux/bluez.md](../protocol/linux/bluez.md#battery-providers)), so two
  LinuxPods on one bus would otherwise share one.
- **A pair's battery goes to the provider its `Device1.Adapter` names:** bluetoothd ignores
  a battery whose device is on another adapter (#51).
- **`AlreadyExists` counts as registered.** The root is named after this connection's
  unique name, so only this connection can hold it. A restart's two owner changes can each
  bring a re-read, and a second registration was refused and never retried (#50).
- **Registered as each adapter is found** - at startup, plugged in later, or there again
  after a restart - and dropped once BlueZ has dropped it. bluetoothd keeps an adapter's
  providers while it is off, so power alone changes nothing.
- **Pairs are attached without a battery** when an adapter refuses, none is found, or a
  pair's adapter cannot be read. They still get AAP, and their keys, without which a pair
  new to the app never shows over BLE.

After every change to the adapters, attachment sweeps the attached pairs again against
what BlueZ reports connected.

## Connections that serve objects

**Built with one served already** (`connection::Builder::serve_at`), never plain (#125).
zbus drops a method call that comes before its object server has subscribed
([protocol/linux/zbus.md](../protocol/linux/zbus.md#the-object-servers-dispatch)). Built
plain, attachment's connection started that server in `BatteryProvider::register`, just
before the registration that bluetoothd answers by reading the root. Under load the read
came first: the fake BlueZ took no battery, and bluetoothd would wait 25 s for its reply.

bluetoothd takes the provider's `InterfacesAdded` while it waits
([protocol/linux/bluez.md](../protocol/linux/bluez.md#battery-providers)), so it would
miss only a battery the root held already, and there is none: attachment registers a
provider before it exports to it. The fake BlueZ, which takes no signal before the reply,
took no battery at all.

So attachment's connection is a `bluez::ProviderConnection`, which only
`bluez::connection` builds, and the fake world's come from `harness::connect_serving`.
The scanner's, which serves its advertisement monitors, is built by `Scanner::connect`
with an ObjectManager served over every monitor root. bluetoothd reads a monitor root
before it answers the registration
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)),
so a read dropped there would leave the scanner waiting for its answer.
The ObjectManager `bluez::connection` serves, over every provider root, is there only to
have something served. It never emits: it is served before any provider root, and each
root serves an ObjectManager of its own nearer the batteries
([protocol/linux/zbus.md](../protocol/linux/zbus.md#which-objectmanager-signals)).

## Subscriptions that advertisements reach

**Drained by a task that never awaits its reader.** Left in zbus's queue, advertisements
stopped zbus reading the connection, and a reply attachment awaited never came (#90;
[protocol/linux/zbus.md](../protocol/linux/zbus.md#queues)).

`bluez::Devices::watch_connections` is one: its rule, every `PropertiesChanged` under
`/org/bluez`, takes in every advertisement after an address's first while the app scans:
under discovery at every one, as the scan sets an LE discovery filter
(`ble::scanner::start_discovery_on`;
[protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery), #71), and through a
monitor at every payload change.

**Coalesced, never queued per signal, never dropped.** What an advertiser or a flapping
adapter can trigger at will is coalesced per device or adapter. Queued one per signal, an
advertiser re-signaling its services kept a pair waiting 55 s to attach in a test (#96).
`Connected` changes are kept, in order.

**A draining task never calls BlueZ.** Awaiting a reply there leaves its own subscription
unread: the #90 stall again. An answer that needs BlueZ is worked out in another task, as
`bluez::watch_adapters` re-reads the adapters.

## Signals in order

**Signals whose order matters come from one match rule, one stream.** zbus queues each
subscription apart, and `select!` over several picks among ready ones at random
([protocol/linux/zbus.md](../protocol/linux/zbus.md#order-across-subscriptions)).
`adapter_changes` reads all of bluetoothd's that way. The fake BlueZ, reading a provider's
added, removed and changed from a stream each, took a battery withdrawn and offered again
in the wrong order (#49).

## Which signals count

A signal matched by `sender=org.bluez` can come from anyone the connection hears, and the
connection watch's rule names no sender at all
([protocol/linux/zbus.md](../protocol/linux/zbus.md#senders)).

- **A BlueZ signal counts only from the unique name the state was read from:** the
  `GetManagedObjects` reply's sender. So does a call to the scanner's monitors: a
  `Release` from anyone else would put an adapter on discovery, and a `DeviceFound`
  have the scanner read a device. The device is read from that unique name, which
  starts nothing.
- **`NameOwnerChanged` counts only from `org.freedesktop.DBus`.**

Applied as they came, an old bluetoothd's last `Powered`, or any local process's, left a
phantom adapter behind (#50).

### Connected is a hint

A `Connected` change only prompts a question (#132):

- Attachment asks BlueZ for `Device1.Connected`, and acts only if the answer agrees. No
  answer reads as disconnected, which never attaches but does detach.
- `attach` reads it once more, last, before the supervisor starts.

Taken as given, a forged `Connected=true` would have had a pair the user disconnected
paged and reconnected ([aap-links.md](aap-links.md#which-pairs)).

### Never starting bluetoothd

A call to `org.bluez` starts bluetoothd under systemd unless it carries `NoAutoStart`
([protocol/linux/bluez.md](../protocol/linux/bluez.md#starting-bluetoothd)). Attachment's
connection handling:

| BlueZ is | Attachment |
|---|---|
| Known to be off the bus | Asks nothing |
| Not known yet | Asks with `NoAutoStart` |
| On the bus | Makes no call for an event BlueZ did not confirm |

So no unconfirmed event starts bluetoothd. The volume and the media pause call without
auto-start too ([volume.md](volume.md), [media-pause.md](media-pause.md)). The calls the
scanner makes for an adapter go to the unique name its adapters were read from, which
nothing starts ([Listening for the pods](#listening-for-the-pods)).

## Tasks that never give up

The tasks on the system bus, `ble_task` and `attachment::run`, never return for good
(#50). They set themselves up again after `bluez::Backoff`, which counts a run shorter than
`bluez::RETRY_MAX` as a failure, so a connection that dies at once is not retried in a
loop. Each used to give up at its first error, and the app stayed deaf until restarted.

`volume::Volume` and `media::Media` have no such loop: they connect when a call needs it,
and again after a connection failed.

## Hardware checks

The advertisement monitor in place of discovery ran in a probe only
([protocol/linux/bluez.md](../protocol/linux/bluez.md#runs)); the scanner's choice has run
against the fake BlueZ only. To try it on a pair, run with
`RUST_LOG=linuxpods::ble=debug`, with `Experimental = true` in `/etc/bluetooth/main.conf`
and without:

- The monitor registered, no discovery (`bluetoothctl show`: `Discovering: no`), and the
  pair's reading over BLE; a bonded mouse switched off and on reconnects.
- Without the experimental interfaces: `BlueZ offers no advertisement monitor on` the
  adapter, once, then `BLE discovery started` there - not a warning repeated on the
  backoff.
- bluetoothd restarted after `Experimental = true` is set, the app still running: the
  monitor taken, discovery stopped.
- Without the experimental interfaces, the Preferences hint's **Pause scanning** button:
  `BLE discovery paused`, the hint's row reading "Paused until" and the time, the mouse
  switched off and on reconnecting, then `BLE discovery pause over`, discovery back and
  the row reading "Scanning". With them, no hint.
- The adapter powered off and on with the monitor registered: whether the pair is still
  heard ([protocol/linux/bluez.md](../protocol/linux/bluez.md#open-questions)).
- GNOME Settings' Bluetooth panel open before the app starts: each pod heard from
  `DeviceFound` alone.
- A BLE spam flood of proximity pairing data, a new address each, while listening
  through a monitor: the pair's readings keep up, and a lid opening shows promptly. Not
  run.
- A pod lost and found again: with `RUST_LOG=linuxpods::ble=trace` and the monitor
  registered, take an idle pair in the open case out of range until `BLE advertisement
  monitor lost <path>` is logged, and bring it back within about 25 s, before bluetoothd
  removes the device. At the next `DeviceFound`, `BLE parsable:` from the read, with no
  payload change.
- `systemctl restart bluetooth` with the monitor registered, under
  `RUST_LOG=linuxpods::ble=debug`: whether a `Release` comes before `NameOwnerChanged` -
  the release warning, then a discovery start that fails or is stopped, or none, as the
  adapter was seen powered off first - and that the
  monitor is registered again under the new bluetoothd, with `Discovering` false at the
  end. With Preferences open, whether the scanning hint shows for that moment, and for
  how long ([ui.md](ui.md#the-scanning-hint)).

Bluetooth off and on, `rfkill`, `systemctl restart bluetooth`, `kill -9 bluetoothd` and a
suspend ran on one adapter with a Pro 3 and a Pro 2 (USB-C) (2026-10-26, PR #121):
[protocol/linux/bluez.md](../protocol/linux/bluez.md#runs).

Still to do, all run against the fake BlueZ only:

- A second adapter, and a USB adapter unplugged while a pair is connected through it: the
  pair's battery moves to, or goes with, its adapter's provider.
- The #90 stall: a BLE spam flood with two connection events close together.
