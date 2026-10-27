# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## Project

LinuxPods manages Apple AirPods on Linux: a GTK4/libadwaita GNOME app with battery
monitoring, a system tray and GNOME Settings integration. Rust, edition 2024, 1.92+.
Dependencies and versions are in `Cargo.toml`; build targets are in the `Makefile`.

**Verify the binary is fresh before testing against hardware.** `cargo test` builds test
harnesses, not `target/debug/linuxpods`; a stale binary has wasted debugging time twice.

## Architecture

```
src/
├── main.rs        # GTK main loop on the main thread; tokio runtime alongside it
├── lib.rs         # Library target so layers can be driven from tests and examples
├── podstate.rs    # Coordinator: merges AAP and BLE, broadcasts snapshots
├── aap/           # Apple Accessory Protocol over L2CAP (PSM 4097): session, decode
├── ble/           # Scanner, Apple Continuity parser, AES decryption
├── bluez.rs       # Adapters, devices, connection watch; a battery provider per adapter
├── attachment.rs  # Which pairs get AAP, and each battery to its adapter's provider
├── keystore.rs    # Encryption keys at ~/.local/share/linuxpods/keys.json (0600)
├── indicator.rs   # System tray via StatusNotifierItem (ksni)
└── ui/            # GTK4/libadwaita: window, Control page, Preferences
```

**Threading.** GTK owns the main thread, tokio carries BLE/AAP/D-Bus. GTK types are
`!Send`, so the two meet over channels - a `tokio::sync::watch` for snapshots,
`async-channel` for commands - consumed with `glib::spawn_future_local`.
Never touch a widget from a tokio task.

**State.** `Coordinator` merges AAP (exact, needs L2CAP) and BLE (10% steps, or exact
once a stored key decrypts) **per device**, keyed by the **real** MAC.

- `supersedes_ble` is per-device, never global - a global pause blanks out every other
  pair - and holds only once the link has a reading. Hidden from link-up, the device
  showed nothing until its first battery packet, and its GNOME Settings battery was
  withdrawn and exported again on every reconnect (#62). It runs *after* decryption,
  since an advertisement carries only a random MAC until then.
- `subscribe()` returns a `watch::Receiver`: one latest-wins value every consumer sees,
  so UI, tray and provider never compete for a snapshot, and a slow one skips stale
  snapshots instead of queueing them (per-consumer unbounded queues grew without limit
  under an advertisement flood). It is marked changed so the current state arrives at
  once - otherwise the window comes up blank until the first advertisement. Never hold
  `borrow()` across an await: every broadcast waits for it while holding the `inner`
  write lock, so a held borrow stalls the whole coordinator, and one held while awaiting
  the coordinator deadlocks. The UI loop pauses `RENDER_INTERVAL` per pass, since tokio's
  coop budget does not reach glib's executor.
- Snapshots go out under the lock they were changed under: `broadcast(&inner)` takes the
  state while the write guard is live, never a snapshot built earlier. Sent after the
  guard was dropped, an update in between could send a newer snapshot first, and the
  older one landed last and stayed on show (#88). `send_replace` never awaits, so holding
  `inner` across it is safe.
- Every attached pair has a supervisor (`attachment::supervise`) that owns its AAP work:
  it opens a link, runs it, and after `bluez::Backoff` opens another when one fails or
  goes. Attachment's event loop only starts and stops supervisors - a setup awaited there
  held every connection, adapter change and battery behind it (#49). `stop` aborts the
  supervisor and awaits its `JoinHandle` before `disconnect_aap`, so the link is gone,
  not going, when it returns. An abort drops the supervisor at whatever await it is in,
  so nothing it awaits may leave work half-done: the keystore save moves its owned lock
  into `spawn_blocking`, where no abort reaches it - aborted mid-save, the lock went
  while the write ran on, and a second save raced it on the same temp file.
- The session itself - connect, handshake, battery and feature requests - is
  `aap::Client::open`. `open_aap` refuses a device that has a link, before and after it,
  requests the keys, and records the link (`Inner::links`: client, reading, mode) as its last step,
  with nothing awaited after, and returns the `AapLink` that owns it. Dropped anywhere -
  timed out, aborted, run to its end - it shuts the socket and removes its own record,
  matched by client, so a newer link stands. Its read loop reads that client only, never
  one looked up by MAC - a loop outliving its link would read the replacement's socket
  under the old identity, which once stored one device's key under another's MAC.
  Commands (`set_noise_control`, `request_encryption_keys`, `disconnect_aap`) name their
  device, and GNOME Settings gets one `BatteryProvider1` object per attached pair.
- Both `open_aap` and `disconnect_aap` broadcast. Disconnect also drops the device's
  AAP state, or the UI keeps showing a stale exact-looking reading.
- BLE is cached, AAP never is. `Inner::ble` keeps the last advertisement per device -
  also the connected one's, underneath AAP - so disconnect falls back to it at once.
  AAP lives in the device's link and goes with it. Identified readings expire after
  `BLE_CACHE_TTL` (30 min) and carry `last_seen` for the UI; `expiry_task` prunes on a
  clock, since with no device in range no advertisement ever triggers a prune.
- Both pods advertise, so one reading never wholly replaces the lid. A worn pod whose
  partner is in the case reports no lid but sets status bit 4 (`one_in_case`), and
  `handle_advertisement` keeps the cached lid for it; with bit 4 clear the lid goes.
  Replaced wholesale, the lid blinked several times a second with one pod worn. The
  parser stays stateless: it reports the bit, the coordinator keeps.
- Apple rotates the advertised BLE MAC every few seconds while disconnected, so an
  advertisement no stored key decrypts cannot be attributed to anything. It is dropped on
  arrival - not stored, not broadcast; storing those is what a BLE spam flood fed on.
  The scanner's `BLE parsable:` log still records it.
- A D-Bus subscription that advertisements reach is drained by a task that never awaits
  its reader, and what an advertiser or a flapping adapter can trigger at will is
  coalesced per device or adapter - never queued per signal, never dropped. Left in
  zbus's 64-message queue, advertisements stopped zbus reading the connection, and a
  reply attachment awaited never came (#90). Queued one per signal, an advertiser
  re-signalling its services kept a pair waiting 55 s to attach in a test (#96).
  `Connected` changes are kept, in order. A draining task only coalesces and never calls
  BlueZ: awaiting a reply leaves its own subscription unread, the #90 stall again. An
  answer that needs BlueZ is worked out in another task, as `bluez::watch_adapters`
  re-reads the adapters.
- A zbus connection that serves objects is built with one served already
  (`connection::Builder::serve_at`), never plain: attachment's is a
  `bluez::ProviderConnection`, which only `bluez::connection` builds, and the fake
  world's come from `harness::connect_serving`. zbus dispatches method calls in a task
  the first `object_server()` spawns, and drops a method call that comes before that task
  has subscribed - no reply, nothing logged above trace. Built plain, attachment's
  connection spawned that task in `BatteryProvider::register`, just before the
  registration BlueZ answers by reading the root. Under load the read came first: the
  fake BlueZ took no battery, and bluetoothd would wait 25 s for its reply (#125).
- A signal matched by `sender=org.bluez` can come from anyone the connection hears: zbus
  cannot check a well-known sender, and the connection watch's rule names no sender at
  all. A BlueZ signal counts only from the unique name the state was read from (the
  `GetManagedObjects` reply's sender), and `NameOwnerChanged` only from
  `org.freedesktop.DBus`. Applied as they came, an old bluetoothd's last `Powered`, or
  any local process's, left a phantom adapter behind (#50).
- Signals whose order matters come from one match rule, one stream: zbus queues each
  subscription apart, and `select!` over several picks among ready ones at random.
  `adapter_changes` reads all of bluetoothd's that way; the fake BlueZ, reading a
  provider's added, removed and changed from a stream each, took a battery withdrawn and
  offered again in the wrong order.
- Adapters are found, never assumed: `bluez::watch_adapters` reads them from
  `GetManagedObjects` and follows them as they power off and on, come and go, and
  bluetoothd restarts (#50). Discovery runs on every powered one, subscribed first, since
  an address's first advertisement comes in the `InterfacesAdded` that announces it. Each
  adapter has a provider root of its own per connection, named after the connection's
  unique name and registered with that adapter's `BatteryProviderManager1`: bluetoothd
  keys providers on the root alone, so two LinuxPods on one bus would otherwise share
  one. A pair's battery goes to the provider its `Device1.Adapter` names: bluetoothd
  ignores a battery whose device is on another adapter. With `/org/bluez/hci0`
  hardcoded, a machine whose adapter was another heard nothing over BLE (#51).
- What a client holds on BlueZ is BlueZ's to end: an adapter powering off ends its
  discovery, one going drops its battery providers, and a bluetoothd restart drops both,
  saying so only by `NameOwnerChanged`. `bluez::watch_adapters` turns that into
  `AdapterSet` tokens, tied to the bluetoothd instance they were read from; consumers
  start again when a token changes, never on a single signal. A provider's `AlreadyExists`
  counts as registered: the root is named after this connection's unique name, so only
  this connection can hold it. A restart's two owner changes can each bring a re-read,
  and a second registration was refused and never retried. Started once at startup,
  discovery went with the first power cycle and the provider with the first restart,
  with nothing in the log (#50).
- `InProgress` from `StartDiscovery` never counts as started. bluetoothd sends it to a
  client that discovers already, and also when the kernel refuses its start as busy,
  dropping the client (`discovery_complete` in BlueZ's `src/adapter.c`). The kernel is
  busy for seconds after `kill -9 bluetoothd`, as the dead daemon's discovery runs on.
  The scanner stops its discovery there, which BlueZ refuses harmlessly if there is none,
  and starts again, and a second refusal waits for the backoff. Taken as started, BLE
  stayed dead until the app restarted (#123).

## Protocol

**Keys are requested on every AAP connection**, not only when none is stored. Re-pairing
regenerates them, and a stale key fails suffix validation without a word, dropping BLE
to 10% steps. The request is one packet, which `open_aap` sends right after
`aap::Client::open`: `open` leaves it out, so a probe never receives keys it did not ask
for and prints them. `aap_read_loop` stores the key that comes back if it is
16 bytes and differs from the one held - the length check keeps a malformed key from
replacing a working one on disk, and skipping an unchanged key avoids a keystore
write per connection.

**BLE validation is by MAC suffix.** Bytes 7-9 of the decrypted payload hold the last
three bytes of the real MAC; a match both validates and identifies. A connected device
zeroes the field, so `00 00 00` is accepted too - identification comes from *which key*
decrypted, not from reading the suffix, and the union costs one bit (2^-23 per wrong key).

⚠️ Do **not** validate with magic bytes. The older check (byte 0 upper nibble `0x0`, byte
4 `0x2D`) holds on neither AirPods Pro 3 (0x2720) nor Pro Gen 2 (0x2420) - both report
byte 4 = `0x1D` - and rejects correct decryptions, silently disabling 1% accuracy.

**AAP packets carry battery only.** `Inner::snapshot` merges in the rest: model and colour
from the identity the link holds; ears, lid, activity and primary pod from the cached
advertisement. The noise control mode is kept on the link (`Link::noise_mode`), apart
from the reading every battery packet replaces, and goes with the link.

**Off needs sub-command 0x34 first.** Recent firmware refuses a bare `0x0D 01` with an
error chime - confirmed on Pro 3 and Pro Gen 2, while the other three modes work
unconditionally. `set_noise_control` sends `0x34 01` immediately before Off, and only for
Off: it is a persistent device setting that syncs to the user's Apple devices, so it is
not something to enable on every connection.

**Noise control is fire-and-forget.** `04 00 04 00 09 00 0D [mode] 00 00 00` sets the
mode; the only consistent answer is a `0x4B` settings-changed notification naming neither
the sub-command nor the mode, and the `0x0D` echo arrives for perhaps one mode in four.
Record the mode optimistically and let a later report confirm it. Validate reports on
byte 4 **and** byte 6: the `0x09` family carries a dozen other sub-commands, all of them
in the startup dump.

**Nothing goes out before the handshake is answered.** The AirPods drop what they are
sent until they have answered it (`01 00 04 00 …`), and a pair that had only just
connected took 1.2 s: requests sent after a fixed 500 ms were never answered, so a pair
connecting while the app ran showed "Connected • waiting for data" with no battery.
`Client::handshake` returns once the answer is in.

**A zero `cid` immediately after `SeqPacket::connect` means the socket is unusable** -
connect returns `Ok` without waiting for the BR/EDR ACL link, every send then fails
`ENOTCONN`, and the socket stays dead even once `cid` becomes nonzero. Discard and retry.

**AAP only to a pair BlueZ reports connected.** An L2CAP connect pages the device and
brings its ACL link up, so a retry to a pair the user has just disconnected would
reconnect it. The supervisor asks `Device1.Connected` before every attempt but the first,
and asks nothing while BlueZ is off the bus: under systemd the call starts it again.

**Never hold the `inner` lock across `read_packet().await`** - clone the link's client out. The read parks until a
packet arrives; this deadlocked the Request Keys button.

## Patterns

- Keep protocol parsing pure and tested. `aap::battery`, `aap::keys`, `aap::noise`,
  `aap::message`, `ble::parser`, `ble::decrypt` take bytes and return values, and carry
  the bulk of the test suite. `aap::decode` routes every packet by its header and opcode;
  each module's `is_*` and `parse_*` still check the header themselves, so they stay safe
  to call on their own.
- Use synthetic test vectors - never commit a real device key. Encrypt in the test and
  assert the round trip.
- `Option<u8>` for unknown battery, never a sentinel. Levels above 100 mean unavailable,
  and so does an AAP part with status Disconnected (`0x04`), whatever its level byte: it
  comes as `0x00` as well as `0xFF`, and the `0x00` read as a real 0% put the case at 0%
  (#108).
- Log stages separately: `BLE parsable:` covers every advertisement that parses, including
  ones the coordinator later drops; `BLE decryptable:` reports identification. Noisy
  non-proximity Apple data goes to `trace`.
- Log text describing control flow goes stale like comments, and is more visible.
- Tasks on the system bus (`ble_task`, `attachment::run`) never return for good: they set
  themselves up again with `bluez::Backoff`, which counts a run under a minute as a
  failure, so a connection that dies at once is not retried in a loop. Each used to give
  up at its first error, and the app stayed deaf until restarted (#50).
- A harness build must behave like a plain one unless the fake world's environment asks
  otherwise (`LINUXPODS_AAP_SOCKET_DIR`, `LINUXPODS_HARNESS_AUTOMATION=1`, set only by
  `scripts/fake-world.sh`): `make test` and `make fake-world` leave it at
  `target/debug/linuxpods`, where it meets real AirPods and the user's session. Nothing in
  the harness writes to the app's real data directory.

**UI.** Widget updates arrive only from the snapshot stream. Read a selection *before*
mutating the widget - splicing a `StringList` makes `GtkDropDown` emit `selected-notify`,
clobbering the user's choice. Noise Control and Features are commands, so they stay
insensitive unless the device is on AAP; BLE is receive-only. Artwork and the app icon
are compiled into a GResource by `build.rs` (`assets/resources.gresource.xml`); a missing
resource fails silently, so a test asserts every name the UI loads is in the bundle. The
tray still points the shell at `assets/icons` in the checkout: the shell draws it and
cannot read resources inside the binary.

## Commits and pull requests

Do **not** put the Claude session URL in commit messages or pull request descriptions -
neither a `Claude-Session:` trailer nor a bare `https://claude.ai/code/session_...` link.
The links resolve for nobody but the author, and they outlive the session they point at.
A `Co-Authored-By:` trailer is fine.

## Documentation

- `docs/ble-proximity-pairing.md` - proximity pairing, decrypted layout, suffix validation
- `docs/aap-key-retrieval.md` - retrieving IRK and ENC_KEY over AAP
- `docs/aap-noise-control.md` - mode packet, the 0x4B notification, the 0x09 family
- `docs/releasing.md` - release steps, what a release contains, verifying one
