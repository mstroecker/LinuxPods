# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## Project

LinuxPods manages Apple AirPods on Linux: a GTK4/libadwaita GNOME app with battery
monitoring, a system tray and GNOME Settings integration. Rust, edition 2024, 1.92+.
Dependencies and versions are in `Cargo.toml`; build targets are in the `Makefile`.

**Verify the binary is fresh before testing against hardware.** Plain `cargo test` builds
test harnesses, not `target/debug/linuxpods`, and `make test` and `make fake-world` leave
a harness build there. Run `cargo build` before testing against a pair; a stale binary
has wasted debugging time twice.

## Architecture

```
src/
├── main.rs        # GTK main loop on the main thread; tokio runtime alongside it
├── lib.rs         # Library target so layers can be driven from tests and examples
├── podstate.rs    # Coordinator: merges AAP and BLE, broadcasts snapshots
├── aap/           # Apple Accessory Protocol over L2CAP (PSM 4097): session, decode
├── ble/           # Scanner, Apple Continuity parser, AES decryption, RPA resolution
├── bluez.rs       # Adapters, devices, connection watch; a battery provider per adapter
├── attachment.rs  # Which pairs get AAP, and each battery to its adapter's provider
├── keystore.rs    # ENC_KEY and IRK per pair at ~/.local/share/linuxpods/keys.json (0600)
├── indicator.rs   # System tray via StatusNotifierItem (ksni)
├── ui/            # GTK4/libadwaita: window, Control page, Preferences, automation
├── harness/       # Harness only: fake AirPods, fake BlueZ, the fake world
└── bin/fakepods.rs # Harness only: the fake world's process, which `make fake-world` runs
```

**Threading.** GTK owns the main thread, tokio carries BLE/AAP/D-Bus. GTK types are
`!Send`, so the two meet over channels: snapshots reach the UI on a `tokio::sync::watch`,
and the tray's window commands on an `async-channel`, both consumed with
`glib::spawn_future_local`. Commands to the AirPods go the other way, onto their link's
queue (`Coordinator::command`), and each result comes back on a oneshot. Never touch a
widget from a tokio task.

**State.** `Coordinator` merges AAP (exact, needs L2CAP) and BLE (exact, and only from an
address a stored IRK resolves, decrypted by that pair's key) **per device**, keyed by the
**real** MAC.

- `supersedes_ble` is per-device, never global - a global pause blanks out every other
  pair - and holds only once the link has a reading. Hidden from link-up, the device
  showed nothing until its first battery packet, and its GNOME Settings battery was
  withdrawn and exported again on every reconnect (#62). It runs *after* identification,
  since an advertisement carries only a random MAC until then.
- `subscribe()` returns a `watch::Receiver`: one latest-wins value every consumer sees,
  so UI, tray and attachment never compete for a snapshot, and a slow one skips stale
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
  requests the keys, and records the link (`Inner::links`: client, reading, mode,
  identity) and its command queue (`Coordinator::queues`) as its last step, with nothing
  awaited after, and returns the `AapLink` that owns it. Dropped anywhere - timed out,
  aborted, run to its end - it shuts the socket and removes its own record and queue,
  matched by client, so a newer link stands. Its read loop reads that client only, never
  one looked up by MAC - a loop outliving its link would read the replacement's socket
  under the old identity, which once stored one device's key under another's MAC.
  `Coordinator::command` and `disconnect_aap` name their device, and GNOME Settings gets
  one `BatteryProvider1` object per attached pair with a reading.
- Commands to the AirPods take one path: `Coordinator::command(mac, Command)`. It queues
  the command on the link's queue in the call itself, not at the first poll, so commands
  go out in the order of the calls however their futures are spawned. Spawned apart, two
  quick switches went out in either order, and the mode recorded last could be the one
  sent first (#54). The queues are a std mutex beside `inner`: taken only under `inner`'s
  write lock or alone, never held across an await, changed only together with
  `Inner::links`, and removed only past `drop_link`'s client check. `AapLink::run`
  carries the commands out one at a time beside the read loop, so they go with the link.
  Each caller gets a `Result`: an error without a link, from the send, or when the link
  goes first - `ConnectionLost` when it went mid-command, as the packet may have gone out.
  Dropping the future does not withdraw the command. The window toasts a failure; the
  tray logs it.
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
- Each pod advertises from a resolvable private address of its own, a new one every 4-8
  minutes, connected or not, and across a closed lid (#71). An advertisement counts only
  if its address resolves with a stored IRK and that pair's ENC_KEY decrypts it; only
  that key is tried (#134). Anything else cannot be attributed to anything and is
  dropped on arrival - not stored, not broadcast; storing those is what a BLE spam flood
  fed on. The scanner's `BLE parsable:` log still records it. Resolved addresses are
  remembered in `rpa::ResolvedAddresses`, bounded and expiring, each with the IRK that
  resolved it; an address no IRK resolves is never remembered, so a spammer rotating
  addresses grows nothing. It is a std mutex beside `inner`, taken alone and never held
  across an await. A pair with no IRK yet, keyed before keystore version 2, is identified
  by its ENC_KEY alone until its next AAP connection stores one.
- A D-Bus subscription that advertisements reach is drained by a task that never awaits
  its reader, and what an advertiser or a flapping adapter can trigger at will is
  coalesced per device or adapter - never queued per signal, never dropped. Left in
  zbus's 64-message queue, advertisements stopped zbus reading the connection, and a
  reply attachment awaited never came (#90). Queued one per signal, an advertiser
  re-signaling its services kept a pair waiting 55 s to attach in a test (#96).
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
  `org.freedesktop.DBus`; the connection watch does not check yet (#132). Applied as they
  came, an old bluetoothd's last `Powered`, or any local process's, left a phantom
  adapter behind (#50).
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
regenerates them, and a stale IRK or key leaves the pair unidentified without a word: it
drops off BLE altogether, since an advertisement nothing identifies is dropped. The request is one
packet, which `open_aap` sends right after `aap::Client::open`: `open` leaves it out, so
a probe never receives keys it did not ask for and prints them. `aap_read_loop` takes the
ENC_KEY and IRK that come back if each is 16 bytes - the length check keeps a malformed
key from replacing a working one. A packet without a usable IRK keeps the stored one
while the ENC_KEY is unchanged; a new ENC_KEY drops it, so the pair falls back to its key
alone rather than vanishing. They go into use at once, and the keystore is written only
when the keys differ from those last saved, so an unchanged key costs no write and a save
that failed is retried (#38). Format version 2 keeps the IRKs in an `irks` map beside
`keys`, so a version 1 file loads unchanged and a version 1 reader still finds its keys.

**BLE identification is by IRK, then ENC_KEY.** The advertising address must resolve
with a stored IRK (`ble::rpa::resolves`, `ah()` of Core spec Vol 3 Part H 2.2.2), and only
that pair's ENC_KEY is tried after it. The key packet carries the IRK **byte-reversed**,
least significant byte first: the keystore keeps it as sent, `resolves` reverses it, and
a test pins the order with the spec's sample (Appendix D.7). Resolving makes a false
match 2^-24 per address and IRK rather than 2^-23 per advertisement and key, and a
replayed advertisement from another address counts for nothing.

The plaintext is then validated by MAC suffix: bytes 7-9 hold the last three bytes of
the real MAC. The pods zero the field unless both are in the case (status bit 2),
connected or not (#71), so `00 00 00` is accepted too - identification comes from the
IRK that resolved and the key that decrypted, not from reading the suffix, and the union
costs one bit (2^-23 per wrong key).

⚠️ Do **not** validate with magic bytes. The older check (byte 0 upper nibble `0x0`, byte
4 `0x2D`) holds on neither AirPods Pro 3 (0x2027) nor Pro 2 USB-C (0x2024) - byte 4 is
part of the last host's address, and byte 0 follows the connection state - and rejects
correct decryptions, leaving every pair unidentified and so dropped.

**AAP packets carry battery only.** `Inner::snapshot` merges in the rest: model and color
from the identity the link holds; ears, lid, activity and primary pod from the cached
advertisement. The noise control mode is kept on the link (`Link::noise_mode`), apart
from the reading every battery packet replaces, and goes with the link.

**Off needs sub-command 0x34 first.** Recent firmware refuses a bare `0x0D 01` with an
error chime - confirmed on Pro 3 and Pro Gen 2, while the other three modes work
unconditionally. `set_noise_control` sends `0x34 01` immediately before Off, and only for
Off: it is a persistent device setting that syncs to the user's Apple devices, so it is
not something to enable on every connection. A link that goes between the two leaves
Off allowed but not set, which is harmless: Off was asked for.

**Noise control is fire-and-forget.** `04 00 04 00 09 00 0D [mode] 00 00 00` sets the
mode, and nothing the device answers confirms it reliably: AirPods 4 sent a `0x4B`
settings-changed notification naming neither sub-command nor mode, and a `0x0D` echo for
perhaps one mode in four; a Pro 3 sent no `0x4B` and echoed every change about 1 s later
(#71). Record the mode optimistically and let a later report confirm it. Validate reports
on the `04 00 04 00` header, byte 4 **and** byte 6: the `0x09` family carries a dozen
other sub-commands, all of them in the startup dump.

A report of the mode a command replaced is held off until the mode set is reported, or
`STALE_REPORT_WINDOW` (2 s) has passed since the command was recorded
(`aap::noise::ModeState`, on the link). After Off a Pro 3 reported the old mode at
+0.33 s and Off at +1.4 s, and taken as it came, the old mode was back on show for a
second (#113). A held report is dropped, not replayed: AirPods 4 also sent the old mode
after a change that took effect, with no echo after it, and the next report - an ear
event's, say - corrects a wrongly dropped one. Past the window any report wins: the mode
may have been changed on another device.

**Nothing goes out before the handshake is answered.** The AirPods drop what they are
sent until they have answered it (`01 00 04 00 …`), and a pair that had only just
connected took 1.2 s: requests sent after a fixed 500 ms were never answered, so a pair
connecting while the app ran showed "Connected • waiting for data" with no battery.
`Client::handshake` returns once the answer is in.

**A zero `cid` after an L2CAP connect means the connect has not finished: never drop a
socket while it is pending.** bluer's connect can return `Ok` with the channel still
connecting: it waits for the socket to poll writable, and a new L2CAP socket polls
writable before it connects. Sends fail `ENOTCONN` until the channel is up. Closed while
connecting, a channel is deleted here without a word to the device (`l2cap_chan_close`
in `BT_CONNECT`), which completes it anyway. Dropped and retried, the retry got the same
local channel, and the pods held two channels to one of ours: the new link's handshake
went unanswered, and when the pods closed the orphan, our kernel closed the live link
(#136). `Client::connect` waits until `L2CAP_CONNINFO` reads back (`BT_CONNECTED`) or
the channel has closed, in a task of its own that the caller's abort or timeout does not
reach: a connect given up on runs on, and a channel that comes up then is closed with the
device told. The task gives up at `CONNECT_LIMIT` (50 s), past the kernel's own 40 s,
and the supervisor's `AAP_CONNECT_TIMEOUT` outlasts both connect and handshake, so its
next attempt never opens a second channel beside a pending one.

**AAP only to a pair BlueZ reports connected.** An L2CAP connect pages the device and
brings its ACL link up, so a retry to a pair the user has just disconnected would
reconnect it. The supervisor asks `Device1.Connected` before every attempt but the first,
and asks nothing while BlueZ is off the bus: under systemd the call starts it again.

**Never hold the `inner` lock across a read or a send on the socket.** `AapLink::run`
hands the link's client to the read loop and the command loop, and neither looks it up
under the lock. A read parks until a packet arrives; holding the lock there once
deadlocked every command sent meanwhile.

## Patterns

- Keep protocol parsing pure and tested. `aap::battery`, `aap::keys`, `aap::noise`,
  `aap::message`, `ble::parser`, `ble::decrypt`, `ble::rpa` take bytes and return values,
  and carry the bulk of the test suite; `noise::ModeState` and `rpa::ResolvedAddresses`
  take the time as an argument for the same reason. `aap::decode` routes every packet by its header and opcode;
  each module's `is_*` and `parse_*` still check the header themselves, so they stay safe
  to call on their own.
- Use synthetic test vectors - never commit a real device key. New test keys come from
  `ble::decrypt::random_key()`, not literals (CodeQL's hard-coded key rule, #66); the
  fixed keys in podstate's and keystore's tests predate that, and the Core spec's public
  RPA sample (Vol 3 Part H Appendix D.7) is marked `gitleaks:allow`. Encrypt in the test
  and assert the round trip.
- `Option<u8>` for unknown battery, never a sentinel. Levels above 100 mean unavailable,
  and so does an AAP part with status Disconnected (`0x04`), whatever its level byte: it
  comes as `0x00` as well as `0xFF`, and the `0x00` read as a real 0% put the case at 0%
  (#108).
- Log stages separately: `BLE parsable:` covers every advertisement that parses, including
  ones the coordinator later drops; `BLE decryptable:` reports identification. Noisy
  non-proximity Apple data goes to `trace`.
- Bytes in logs and probe output go through `linuxpods::hex`, and a packet that may carry
  keys through `aap::keys::redacted_hex`, which shows a key packet's preamble only. No
  log, probe or example prints key bytes: their output gets pasted into issues.
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

**UI.** Widget updates arrive only from the snapshot stream. The one exception is a
command that fails: its result comes back to the main context and goes up as a toast, an
event, never a control's state, so the noise radios keep showing the recorded mode. Read
a selection *before* mutating the widget - splicing a `StringList` makes `GtkDropDown`
emit `selected-notify`, clobbering the user's choice. Noise Control is a command,
so it stays insensitive unless the device is on AAP, and Features stays insensitive until
the commands behind it are known; BLE is receive-only.
Artwork and the app icon are compiled into a GResource by `build.rs`
(`assets/resources.gresource.xml`); a missing resource fails silently, so a test asserts
every name the UI loads is in the bundle. The tray still points the shell at
`assets/icons` in the checkout: the shell draws it and cannot read resources inside the
binary.

## Commits and pull requests

Do **not** put the Claude session URL in commit messages or pull request descriptions -
neither a `Claude-Session:` trailer nor a bare `https://claude.ai/code/session_...` link.
The links resolve for nobody but the author, and they outlive the session they point at.
A `Co-Authored-By:` trailer is fine.

## Documentation

- `docs/ble-proximity-pairing.md` - proximity pairing, identification by IRK then ENC_KEY,
  decrypted layout, suffix validation
- `docs/aap-key-retrieval.md` - retrieving IRK and ENC_KEY over AAP
- `docs/aap-noise-control.md` - mode packet, the 0x4B notification, the 0x09 family
- `docs/releasing.md` - release steps, what a release contains, verifying one
