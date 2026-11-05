# AAP links

How LinuxPods keeps an AAP link to each connected pair: which pairs get one, the
supervisor that keeps it up, how a link opens, and how it ends. The session itself is
the pods': [protocol/aap/session.md](../protocol/aap/session.md). The kernel's part:
[protocol/linux/l2cap.md](../protocol/linux/l2cap.md).

## Which pairs

**AAP only to a pair BlueZ reports connected.** An L2CAP connect pages the device and
brings its link up ([protocol/linux/l2cap.md](../protocol/linux/l2cap.md#connecting)), so
a retry to a pair the user has just disconnected would reconnect it.

- **Attached:** a pair is attached while BlueZ reports it connected and it lists the
  AirPods service (`bluez::is_airpods`), never by its alias, which is the user's to change.
- **Started on BlueZ's word.** `attach` starts the pair's supervisor only on BlueZ's own
  answer that the pair is connected, never a signal's, and reads it once more, last
  ([bluez.md](bluez.md#connected-is-a-hint)).
- **Asked again before every attempt but the first.** The supervisor reads
  `Device1.Connected` first, and asks nothing while BlueZ is off the bus: under systemd a
  call would start it again ([protocol/linux/bluez.md](../protocol/linux/bluez.md#starting-bluetoothd)).

## The supervisor

Every attached pair has a supervisor (`attachment::supervise`), a task that owns its AAP
work. It opens a link, runs it, and opens another after `bluez::Backoff` when one fails
to come up, or goes while BlueZ still reports the pair connected.

- **Attachment's event loop only starts and stops supervisors** (#49). A setup awaited
  there held every connection, adapter change and battery behind it. Now pairs connect in
  parallel.
- **`stop` aborts the supervisor and awaits its `JoinHandle` before `disconnect_aap`,** so
  the link is gone, not going, when it returns. Once a pair is detached, nothing links it
  again.
- **Nothing a supervisor awaits may leave work half done.** An abort drops the supervisor
  at whatever await it is in. So the keystore's save runs on the blocking pool with its
  lock moved in ([keystore.md](keystore.md#written-only-when-changed)), and a pending
  L2CAP connect runs on in a task of its own ([below](#connecting)).
- **Nothing that must outlive a link runs in a supervisor:** putting the volume back and
  the media pause have tasks of their own ([volume.md](volume.md), [media-pause.md](media-pause.md)).

## Opening a link

`aap::Client::open` opens the session: the connect, the handshake, the battery and
feature requests. `Coordinator::open_aap` wraps it:

1. Refuses a pair that has a link.
2. Opens the session.
3. Sends the key request ([keystore.md](keystore.md)). A failure there does not fail the
   link: only BLE needs the keys.
4. Refuses the pair again if another link came up meanwhile, and closes its own.
5. Records the link (`Inner::links`: the client, its reading, mode, settings, name, ear
   state and identity) and its command queue (`Coordinator::queues`), tells `LinkUp`
   ([state.md](state.md#events)) and broadcasts. This is the last step, and nothing is
   awaited after it.
6. Returns the `AapLink` that owns the record.

**Nothing is awaited between recording the link and handing out its `AapLink`.** Only the
`AapLink`'s drop removes the record and the queue. An attempt aborted or timed out at an
await in between would leave a record and a queue that nothing ever removes: the pair would
read as linked, refuse every new link, and take commands that nobody carries out.

`AAP_CONNECT_TIMEOUT` bounds the whole attempt. It outlasts `CONNECT_LIMIT` and
`HANDSHAKE_TIMEOUT` together, so the next attempt never opens a second channel beside a
pending one.

### Connecting

What a pending connect looks like, and what dropping one did (#136):
[protocol/linux/l2cap.md](../protocol/linux/l2cap.md#connecting).

**Never drop a socket while its connect is pending.** `Client::connect` waits until
`L2CAP_CONNINFO` reads back or the channel has closed, polling in a task of its own that
the caller's abort or timeout does not reach. A connect given up on runs on, and a channel
that comes up then is closed with the device told. The task gives up at `CONNECT_LIMIT`,
past the kernel's own limit.

### The handshake

Why nothing goes out before the answer:
[protocol/aap/session.md](../protocol/aap/session.md#step-2-send-handshake).

`Client::handshake` returns once the answer is in, and fails if none comes within
`HANDSHAKE_TIMEOUT`. A fixed 500 ms pause before the requests left a pair that had just
connected at "Connected • waiting for data", with no battery (PR #109).

### The features packet

What the `0x4D` packet is for:
[protocol/aap/session.md](../protocol/aap/session.md#prerequisites-the-features-packet).

Every link sends it as it comes up (`aap::noise::ENABLE_FEATURES`).

## Running and ending

`AapLink::run` hands the link's client to the read loop and to the command loop
([commands.md](commands.md)), and runs both until a read fails or the link is closed.

- **The read loop reads its own client, never one looked up by MAC.** A loop outliving its
  link would read the replacement's socket under the old identity: that once stored one
  device's key under another's MAC.
- **What a loop or a command writes goes through `Inner::live_link`,** which takes the
  link only while it is still that client's. A packet read just before the link was
  replaced must not land on the new one.
- **Never hold the `inner` lock across a read or a send on the socket.** A read parks until
  a packet arrives; holding the lock there once deadlocked every command sent meanwhile.
- **Dropped anywhere** - timed out, aborted, run to its end - the `AapLink` shuts the
  socket and removes its own record and queue, matched by client, so a newer link stands.
- **`disconnect_aap` closes the pair's link** and broadcasts, and the pair's AAP state goes
  with it ([state.md](state.md#what-a-snapshot-shows)).

GNOME Settings gets one battery per attached pair with a reading:
[battery.md](battery.md#gnome-settings).

## Hardware checks

None open. The pending connect waited for ran on a Pro 3 and a Pro 2 (USB-C)
(2026-10-29, PR #137): [protocol/linux/l2cap.md](../protocol/linux/l2cap.md#runs).
