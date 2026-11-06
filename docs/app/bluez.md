# BlueZ and D-Bus

How LinuxPods works with bluetoothd over D-Bus: finding the adapters and following them,
discovery, the battery providers, the subscriptions that advertisements reach, and which
signals it believes. The code is in `bluez`, `ble::scanner` and `attachment`. What
bluetoothd and zbus do, with their sources:
[protocol/linux/bluez.md](../protocol/linux/bluez.md),
[protocol/linux/zbus.md](../protocol/linux/zbus.md).

## Adapters

**Found, never assumed** (#51). `bluez::watch_adapters` reads the adapters from
`GetManagedObjects` and follows them as they power off and on, come and go, and
bluetoothd restarts (#50). With `/org/bluez/hci0` hardcoded, a machine whose adapter was
another heard nothing over BLE.

**What a client holds is BlueZ's to end:** an adapter powering off ends its discovery, one
going drops its battery providers, and a restart drops both, saying so only by
`NameOwnerChanged` ([protocol/linux/bluez.md](../protocol/linux/bluez.md#adapters-and-restarts)).
Started once at startup, discovery went with the first power cycle and the provider with
the first restart, with nothing in the log (#50).

So `watch_adapters` turns what it hears into `AdapterSet` tokens, each tied to the
bluetoothd instance it was read from. A consumer starts again when the token changes,
never on a single signal.

## Discovery

**On every powered adapter, subscribed first.** An address's first advertisement comes in
the `InterfacesAdded` that announces it. With nobody else discovering, every pod's is
such a first: bluetoothd removes the pods' devices when the last discovery ends, so a
start hears each pod at once (#153;
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

**Discovery that never ends keeps some devices from connecting** (#189). While it runs,
the kernel's background scan is off, and a device that reconnects by directed
advertising - a bonded mouse, say - stays unconnected until discovery stops
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising)).
The scanner holds discovery for as long as the app runs, so the app keeps such a device
unconnected for as long as it runs. An advertisement monitor delivers the pods' payload
and leaves such a device free to connect
([protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery-and-directed-advertising));
whether it can take discovery's place waits on its
[open questions](../protocol/linux/bluez.md#open-questions).

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
The ObjectManager `bluez::connection` serves, over every provider root, is there only to
have something served. It never emits: it is served before any provider root, and each
root serves an ObjectManager of its own nearer the batteries
([protocol/linux/zbus.md](../protocol/linux/zbus.md#which-objectmanager-signals)).

## Subscriptions that advertisements reach

**Drained by a task that never awaits its reader.** Left in zbus's queue, advertisements
stopped zbus reading the connection, and a reply attachment awaited never came (#90;
[protocol/linux/zbus.md](../protocol/linux/zbus.md#queues)).

`bluez::Devices::watch_connections` is one: its rule, every `PropertiesChanged` under
`/org/bluez`, takes in every advertisement after an address's first while the app scans,
as the scan sets an LE discovery filter (`ble::scanner::start_discovery_on`;
[protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery), #71).

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
  `GetManagedObjects` reply's sender.
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
auto-start too ([volume.md](volume.md), [media-pause.md](media-pause.md)).

## Tasks that never give up

The tasks on the system bus, `ble_task` and `attachment::run`, never return for good
(#50). They set themselves up again after `bluez::Backoff`, which counts a run shorter than
`bluez::RETRY_MAX` as a failure, so a connection that dies at once is not retried in a
loop. Each used to give up at its first error, and the app stayed deaf until restarted.

`volume::Volume` and `media::Media` have no such loop: they connect when a call needs it,
and again after a connection failed.

## Hardware checks

Bluetooth off and on, `rfkill`, `systemctl restart bluetooth`, `kill -9 bluetoothd` and a
suspend ran on one adapter with a Pro 3 and a Pro 2 (USB-C) (2026-10-26, PR #121):
[protocol/linux/bluez.md](../protocol/linux/bluez.md#runs).

Still to do, all run against the fake BlueZ only:

- A second adapter, and a USB adapter unplugged while a pair is connected through it: the
  pair's battery moves to, or goes with, its adapter's provider.
- The #90 stall: a BLE spam flood with two connection events close together.
