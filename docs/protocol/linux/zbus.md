# zbus

What zbus 5 does that a D-Bus client built on it has to know: its message queues, when
its object server starts answering, which ObjectManager signals for an object, what a
match rule's sender checks, and order across subscriptions. Most turned up as bugs in the pipeline tests against the fake BlueZ, the
senders in review (#50, #132); each was then read in zbus 5.19.0's source. None needed
hardware.

The app's D-Bus work is in `bluez`, `attachment` and `ble::scanner`; the rules built on
these facts are in [app/bluez.md](../../app/bluez.md).

## Queues

- **64 messages per subscription.** zbus queues each subscription's messages apart, 64
  at most unless asked for more (`DEFAULT_MAX_QUEUED` in zbus 5.19.0's
  `src/connection/mod.rs`).
- **A full queue stops the connection.** When a queue is full, zbus reads nothing more
  from the connection until there is room (`src/connection/socket_reader.rs`, #90).
- **Replies wait behind it.** A method reply comes in through the same reader. A task
  that awaits a reply while its own subscription goes unread waits for good (#90).

Seen in the pipeline tests (df607fa, #90): a flood of advertisements filled the queue of
a subscription to every `PropertiesChanged` under `/org/bluez`, and the next call on that
connection never got its reply, three runs out of three.

## The object server's dispatch

- **Started by the first `object_server()`.** zbus dispatches method calls in a task
  that the connection's first `object_server()` call spawns (`start_object_server` in
  zbus 5.19.0's `src/connection/mod.rs`).
- **A call before that is dropped.** A method call that arrives before that task has
  subscribed gets no reply and no error, and nothing is logged above trace (#125,
  52e5c5c).
- **Signals are not.** The task subscribes to method calls alone, and the connection's
  reader hands each message to every subscription whose rule matches it, so a signal
  reaches its own subscriptions whenever the task starts (`start_object_server` in zbus
  5.19.0's `src/connection/mod.rs`, `receive_msg` in `src/connection/socket_reader.rs`).
- **Built serving, it misses nothing.** A connection built with an object served already
  (`connection::Builder::serve_at`) has the task subscribed before it reads its first
  message (52e5c5c, #125).

Seen in the pipeline tests under load (#125): the fake BlueZ's `GetManagedObjects` of a
new battery provider's root - the call bluetoothd makes as it takes a registration - went
unanswered in 5 of 120 runs on a connection built plain, and never on one built
serving. What bluetoothd does while it waits: [bluez.md](bluez.md#battery-providers).

## Which ObjectManager signals

- **The nearest one above the object.** An interface added at a path or removed from it
  is signaled (`InterfacesAdded`, `InterfacesRemoved`) from the closest object above that
  path that serves `org.freedesktop.DBus.ObjectManager`, and from no other
  (`Node::get_child_mut` in zbus 5.19.0's `src/object_server/node.rs`, used by
  `ObjectServer::add_arc_interface` and `remove_named` in `src/object_server/mod.rs`).
- **One further up stays silent** for every object that has a nearer one, with one
  exception:
- **An ObjectManager added at a path signals every object below it.** It sends one
  `InterfacesAdded` for each, at once, from its own path, past any nearer ObjectManager
  (`ObjectServer::add_arc_interface` in `src/object_server/mod.rs`, which walks them with
  `Node::get_managed_objects` in `src/object_server/node.rs`). With no object below, it
  sends nothing.

Read in the source, not tested on its own.

## Senders

- **A message's sender is always a unique name.** So zbus cannot check a rule's
  well-known sender, such as `sender=org.bluez`, against a message, and lets it match
  (`MatchRule::matches` in zbus 5.19.0's `src/match_rule/mod.rs`).
- **A stream subscribed with `sender=org.bluez` can get anyone's signal** that the
  connection hears: one let in by another rule on the connection that names no sender
  (#132), or the last signals of a bluetoothd that has just gone (#50).
- **Whether anyone may send such a signal** depends on the system bus policy. The
  default `system.conf` generally lets clients send signals (#132).
- **A reply names who answered.** On a message bus, a message's `SENDER` header field is
  the unique name of the sending connection, set by the bus itself. So the reply to
  `GetManagedObjects` names bluetoothd's unique name (D-Bus Specification 0.43, "Header
  Fields").
- **`NameOwnerChanged` comes from `org.freedesktop.DBus`**, the name the bus itself owns
  (D-Bus Specification 0.43, "Message Bus Specification").

How bluetoothd's restarts show: [bluez.md](bluez.md#adapters-and-restarts).

## Order across subscriptions

- **Each subscription is queued apart.** zbus's reader takes each message off the
  socket in turn and puts it on every queue whose rule it matches (`receive_msg` in zbus
  5.19.0's `src/connection/socket_reader.rs`). Nothing keeps the order between the
  messages of two queues.
- **`select!` picks at random.** tokio's `select!` over several ready branches picks the
  first one to check at random by default (tokio 1.54.1, `select!` documentation,
  "Fairness").
- **One match rule, one stream, keeps the order** in which the connection read the
  signals: one queue gets them in read order (`src/connection/socket_reader.rs`, zbus
  5.19.0; 4ec4144, #49).

Seen in the pipeline tests (4ec4144, #49): the fake BlueZ read a provider's
`InterfacesAdded`, `InterfacesRemoved` and `PropertiesChanged` from a stream each. A
battery withdrawn and offered again within a moment was taken the other way round.

## Property setters

A property setter written with `#[zbus(property)]` in an `#[interface]` impl signals
every set it answers with `Ok`. The code `#[interface]` generates calls the property's
`<name>_changed` right after the setter returns `Ok`, and `<name>_changed` reads the
property's getter again and sends `PropertiesChanged` with what it returns (zbus_macros
5.19.0, `src/iface.rs`). So a set that leaves the value as it was is signaled all the
same, with the value unchanged. A setter that returns an error sends nothing. This holds
for a property that emits `PropertiesChanged` with its value, the default
(`emits_changed_signal`).

## Open questions

- The stall of #90 has not been seen on hardware: it needs a crowd or a BLE spam flood,
  and two connection events close together (#90).
