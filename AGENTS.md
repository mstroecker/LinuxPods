# AGENTS.md

Guidance for coding agents working in this repository. Claude Code (claude.ai/code) reads
it as `CLAUDE.md`, a link to this file: edit `AGENTS.md`.

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
├── readings.rs    # Each pair's last BLE reading across a relaunch, in readings.json beside the keys
├── media.rs       # The media paused when a pod leaves an ear, played again once back (MPRIS)
├── settings.rs    # Preferences' settings at ~/.config/linuxpods/settings.json
├── indicator.rs   # System tray via StatusNotifierItem (ksni)
├── notify.rs      # Low battery notifications: what to warn about, once per crossing
├── volume.rs      # The pair's volume lowered while the user speaks (MediaTransport1)
├── ui/            # GTK4/libadwaita: window, Control page, rename dialog, Preferences, notifications, automation
├── harness/       # Harness only: fake AirPods, fake BlueZ, a fake media player, the fake world
└── bin/fakepods.rs # Harness only: the fake world's process, which `make fake-world` runs
```

**Threading.** GTK owns the main thread, tokio carries BLE/AAP/D-Bus. GTK types are
`!Send`, so the two meet over channels: snapshots reach the UI on a `tokio::sync::watch`,
and the tray's window commands, the low battery notifications and the signal task's quit
request (SIGTERM, SIGINT) on an `async-channel`, all consumed with
`glib::spawn_future_local`. Commands to the AirPods go the other way,
onto their link's queue (`Coordinator::command`), and each result comes back on a oneshot.
The Preferences switch for pausing on the ears goes that way too, to `media::Media` on a
`watch`.
Never touch a widget from a tokio task.

**State.** `Coordinator` merges AAP (exact, needs L2CAP) and BLE (exact, and only from an
address a stored IRK resolves, decrypted by that pair's key) **per device**, keyed by the
**real** MAC.

- `supersedes_ble` is per-device, never global - a global pause blanks out every other
  pair - and holds only once the link has a reading. Hidden from link-up, the device
  showed nothing until its first battery packet, and its GNOME Settings battery was
  withdrawn and exported again on every reconnect (#62). It runs *after* identification,
  since an advertisement carries only a random MAC until then.
- `subscribe()` returns a `watch::Receiver`: one latest-wins value every consumer sees,
  so UI, tray, attachment and the notifier never compete for a snapshot, and a slow one
  skips stale snapshots instead of queueing them (per-consumer unbounded queues grew
  without limit under an advertisement flood). It is marked changed so the current state
  arrives at once - otherwise the window comes up blank until the first advertisement.
  Never hold `borrow()` across an await: every broadcast waits for it while holding the
  `inner` write lock, so a held borrow stalls the whole coordinator, and one held while
  awaiting the coordinator deadlocks. The UI loop pauses `RENDER_INTERVAL` per pass, since tokio's
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
  The speech-event and ear-event consumers are a std mutex each beside `inner`, taken
  under `inner`'s write lock to tell an event, so a link's levels and places come between
  its coming up and its going, or alone to register; never held across an await.
  Each caller gets a `Result`: an error without a link, from the send, or when the link
  goes first - `ConnectionLost` when it went mid-command, as the packet may have gone out.
  Dropping the future does not withdraw the command. The window toasts a failure; the
  tray logs it.
- Both `open_aap` and `disconnect_aap` broadcast. Disconnect also drops the device's
  AAP state - all but the firmware version - or the UI keeps showing a stale
  exact-looking reading.
- BLE is cached, AAP never is - but for the firmware version, the pair's rather than the
  link's: `Inner::firmware_versions` keeps the last each pair's links reported, for the
  app's run, never pruned, and the next link's replaces it (#161). `Inner::ble` keeps
  the last advertisement per device - also the connected one's, underneath AAP - so
  disconnect falls back to it at once. AAP lives in the device's link and goes with it.
  Identified readings expire after `BLE_CACHE_TTL` (a day, #169) and carry
  `last_seen` for the UI; `expiry_task` prunes on a clock, since with no device in
  range no advertisement ever triggers a prune.
- The BLE cache's ages are on the wall clock (`SystemTime`, `podstate::age`), never
  `Instant`: that is `CLOCK_MONOTONIC`, which stands still while the machine sleeps, so
  a reading from before a night's suspend read "30 min ago" the next morning and
  expired hours late, and a time kept across a relaunch has to mean something to the
  next run. A reading heard after now - the clock set back - has no age: it goes at the
  next prune and on load, and is never fresh for a notification. Every other `Instant`
  (AAP, ear, noise, volume, media) stays one.
- The cache outlives the app (#170): `readings` keeps each cached reading's levels,
  charging flags, model and heard time - not in-ear, the lid or the connection state,
  stale by the time anything loads them - in `readings.json` beside the keys (0600,
  temp file and rename, as the keystore). Loaded at startup, only for a pair with keys
  and heard less than `BLE_CACHE_TTL` ago; a file that does not parse is set aside, and
  a level above 100 loads as unknown. A loaded reading is `PodState::restored` until the
  pair is heard again: shown, with its age, but left out of GNOME Settings by
  `attachment::exported_levels`, as `Battery1` would show yesterday's level as now -
  never by a freshness window there, which would withdraw and export it again (#62).
  Never saved per advertisement: `expiry_task` calls `Coordinator::save_readings` each
  minute, which writes only when `readings::due` says so - a pair come or gone, a level,
  flag or model changed, or a heard time moved `readings::HEARD_REFRESH` - and `main.rs`
  once more at the quit, heard times as they are, waited for without a bound: a small
  file on the local disk, as the keys are, and dropping the runtime waits for the
  blocking pool anyway. The save copies the readings out under `inner`'s read lock and
  writes on the blocking pool with its owned lock moved in, so an abort never leaves it
  half done. The readings lock, like the keystore's, is taken before `inner`, never
  under it. The store sits in the keystore's directory, so a test's or the fake world's
  keystore keeps it out of the user's.
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
  `org.freedesktop.DBus`. Applied as they came, an old bluetoothd's last `Powered`, or
  any local process's, left a phantom adapter behind (#50). A `Connected` change is only
  a hint: attachment asks BlueZ for `Device1.Connected` and acts only if the answer
  agrees - no answer reads as disconnected, which never attaches but does detach - and
  `attach` reads it once more, last, before the supervisor starts. Attachment's
  connection handling asks nothing while it knows BlueZ is off the bus, asks with
  `NoAutoStart` for the moment before it knows, and makes no other call for an event
  BlueZ did not confirm, so no such event starts bluetoothd. Taken as given, a forged
  `Connected=true` would have a pair the user disconnected paged and reconnected (#132).
- Signals whose order matters come from one match rule, one stream: zbus queues each
  subscription apart, and `select!` over several picks among ready ones at random.
  `adapter_changes` reads all of bluetoothd's that way; the fake BlueZ, reading a
  provider's added, removed and changed from a stream each, took a battery withdrawn and
  offered again in the wrong order.
- Adapters are found, never assumed: `bluez::watch_adapters` reads them from
  `GetManagedObjects` and follows them as they power off and on, come and go, and
  bluetoothd restarts (#50). Discovery runs on every powered one, subscribed first, since
  an address's first advertisement comes in the `InterfacesAdded` that announces it. With
  nobody else discovering, every pod's is such a first: bluetoothd removes the pods'
  devices when the last discovery ends, so a start hears each pod at once (#153). Each
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

**Say where a reading comes from.** A protocol reading taken from LibrePods, or guessed,
says so where it is written - doc, comment, example, README - until a run here confirms
it, and names that run once one does. The magic-byte check and the `0x4B`
"settings-changed notification" were each a guess written down as fact, and each stood
until hardware showed otherwise: the check rejected every correct decryption, and the
notification was Conversation Awareness's speech level `09` (#150).

**Keys are requested on every AAP connection**, not only when none is stored. What would
change them is not known - a Pro 2 forgotten and paired again with the same computer
kept both (2026-11-01) - and a stale IRK or key leaves the pair unidentified without a
word: it drops off BLE altogether, since an advertisement nothing identifies is dropped.
The request is one packet, which `open_aap` sends right after `aap::Client::open`:
`open` leaves it out, so a probe never receives keys it did not ask for and prints them.
`aap_read_loop` takes the ENC_KEY and IRK that come back if each is 16 bytes - the
length check keeps a malformed key from replacing a working one. A packet without a
usable IRK keeps the stored one while the ENC_KEY is unchanged; a new ENC_KEY drops it,
so the pair falls back to its key alone rather than vanishing. They go into use at once,
and the keystore is written only when the keys differ from those last saved, so an
unchanged key costs no write and a save that failed is retried (#38). Format version 2
keeps the IRKs in an `irks` map beside `keys`, so a version 1 file loads unchanged and a
version 1 reader still finds its keys.

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

**AAP packets carry battery and in-ear.** In-ear comes unasked on every change, in two
packets: the ear report (`0x06`) gives the primary pod's place first, so it reads only
with the primary pod packet (`0x08`). The link keeps both (`Link::ear`, an
`aap::ear::EarState`), and in-ear comes from them once it has both - from the cached
advertisement before that and after the link goes. Never take that primary from the
advertisement: each pod calls itself primary in its own (#71), and a connected pair's
payload changes seldom and lags its ears - 4 reached the app in 70 s (#142). At a role switch the `0x06` in the new order comes up to 31 ms before the
`0x08` on a Pro 3 (#142) and up to 87 ms on a Pro 2 (USB-C) (#71), and read with the old primary it swaps the pods; `EarState` holds such a report
for the `0x08`, at most `ROLE_SWITCH_HOLD`. `Inner::snapshot` merges in the rest: model
and color from the identity the link holds; lid and activity from the cached
advertisement. A snapshot shows no primary pod: the advertisement's flag says only which
pod was heard last, and for a pair on AAP every advertisement from the other pod broadcast
a snapshot that differed in nothing else (#152). The noise control mode is kept on the
link (`Link::noise_mode`), apart from the reading every battery packet replaces, and goes
with the link like `Link::ear`. Each change of a pod's place in `Link::ear` also goes out
as an `EarEvent`, for pausing the media - see below.

**Off needs sub-command 0x34 first.** Recent firmware refuses a bare `0x0D 01` with an
error chime - confirmed on Pro 3 and Pro Gen 2, while the other three modes work
unconditionally. `set_noise_control` sends `0x34 01` immediately before Off, and only for
Off: it is a persistent device setting that syncs to the user's Apple devices, so it is
not something to enable on every connection. A link that goes between the two leaves
Off allowed but not set, which is harmless: Off was asked for.

**Noise control is fire-and-forget.** `04 00 04 00 09 00 0D [mode] 00 00 00` sets the
mode, and nothing the device answers confirms it reliably: AirPods 4 sent a `0x0D` echo
for perhaps one mode in four, and a Pro 3 echoed every change about 1 s later (#71). The
`0x4B` that AirPods 4 sent after every change, naming neither sub-command nor mode, is
Conversation Awareness's level `09`, and comes only with that setting on: a Pro 3 with it
on answered every change `08`, the echo, `09` (#150). Record the mode optimistically and
let a later report confirm it. Validate reports on the `04 00 04 00` header, byte 4
**and** byte 6: the `0x09` family carries a dozen other sub-commands, all of them in the
startup dump.

A report of the mode the last command replaced is held off until the mode set is
reported, or `STALE_REPORT_WINDOW` (2 s) has passed since the command was recorded
(`aap::noise::ModeState`, on the link). After Off a Pro 3 reported the old mode at
+0.33 s and Off at +1.4 s, and taken as it came, the old mode was back on show for a
second (#113). A held report is dropped, not replayed: AirPods 4 also sent the old mode
after a change that took effect, with no echo after it, and the next report - an ear
event's, say - corrects a wrongly dropped one. Past the window any report wins: the mode
may have been changed on another device. Only the last command's replaced mode is held
off, and that is accepted: a report of a mode an earlier command set or replaced is
taken, so quick successive changes can briefly replay older modes, and the last
command's echo settles it, so the mode on show always ends as the one last set.

**Conversation Awareness is a setting; its levels are speech.** `0x09` sub-command `0x28`
(`01` on, `02` off) is reported in the startup dump, and the same packet sets it (a
Pro 3, 2026-11-02). That it is not reported again when it changes is LibrePods' word, not
yet checked here (#150), so `Command::ConversationAwareness` is recorded optimistically
like the mode, on the link (`Link::conversation_awareness`), and goes with it; a change
made on a phone may never show until the next connection. With it on the pods send
`04 00 04 00 4B 00 02 00 01 [level]`: `01`/`02` speaking, `03`-`07` easing back, `08`
normal, `09` idle - a target, not a step to count. `08` and `09` also frame every mode
change, so they name no mode and acknowledge nothing. While the user speaks the pods
switch to Transparency and back themselves, as ordinary mode reports, taken like any
other. The link's last level is in the snapshot (`PodState::speech_level`), and every
level goes to the volume as an event.

**Rename is fire and forget, and never touches BlueZ's alias.**
`04 00 04 00 1A 00 01 [length] 00 [UTF-8 name]`, the length in bytes, renames the pods -
the name they report to a host as it connects, in the device information packet (`0x1D`).
Nothing answers it (a Pro 3, #160; `docs/protocol/aap/rename.md` has both runs): `Command::Rename`
records the name on the link (`Link::name`) once the packet is away, and it goes with the
link. The pods keep 127 bytes and cut a longer name silently, so a name that is empty,
has a NUL or runs over 127 bytes is refused, never sent - by the dialog, by
`Coordinator::command` before it queues, and by `aap::name` - and never cut by bytes
before it is sent; the entry's `max-length` stops a text at 127 characters.
`Device1.Alias` is never set or cleared (`docs/app/names.md` says why). `0x1D` is read
only with `02` at byte 6 and `04 00` at bytes 9-10, and the rest of it holds serial
numbers: only the name and the firmware version are read or logged (`aap::info`), and
`aap::keys::redacted_hex` cuts any `0x1D` to its preamble.

**The firmware version is `0x1D`'s field 10, packed.** The dissector's Marketing Version,
a decimal string: `9442752` = `0x9015C0` on a Pro 3 and a Pro 2 (USB-C) on 2026-11-02,
when the iPhone showed `9A348` for both. `aap::info::FirmwareVersion` reads bits 20 and
up as the major, 16-19 as a letter (`A` = 0), 4-15 as the build in decimal, and the low
nibble as a suffix letter (`a` = 1, none for 0) - ours, read off that one value, and
the suffix and a major past 9 both guesses on top, all to be confirmed at the next
update (`docs/protocol/aap/device-info.md`). Field 10's value is logged beside the reading, so
a wrong one shows. Each field of `0x1D` carries its own error (`aap::info::DeviceInfo`):
a name that is not UTF-8 leaves the version, and a version that does not read leaves
the name; an error never carries the field's bytes, as one out of place could be a
serial number.

**The volume lowered while the user speaks is the pair's own, through BlueZ.** The
pair's A2DP transport's `org.bluez.MediaTransport1.Volume` (0-127, AVRCP absolute
volume), never the default output, never PipeWire, never a volume in software: set by
hand on a Pro 3, PipeWire's sink followed (#150); GNOME's slider, which follows the sink,
was not watched. bluetoothd checks no owner for `Volume`, whatever its documentation
says. `volume::Ducking` decides, pure and taking the time; `volume::Volume` carries it
out in a task of its own.
- The transport's path is new with every A2DP setup (`<device>/sep1/fd<n>`, `n` the
  first free from the count of all bluetoothd's transports, BlueZ 5.87 `transport.c`),
  so it is looked up under the pair's device path every time - each level that may set
  something, each fade step - by `Device` and `UUID`, and never kept. No transport,
  nothing to lower.
- Lowered only while that A2DP transport is `active`. A pair nothing plays to is left
  alone, and so, assumed, is one in a call: whatever runs the headset profile is assumed
  to leave A2DP not `active`, released or gone - not read in any source, not watched
  during a call here; calls have run in the fake world only.
- Down at once to `volume::LOWERED_PERCENT` of the volume before, at the first `01`/`02`;
  back over `volume::FADE` in steps from the first level of `03` or above, whatever comes
  meanwhile; a `01`/`02` during the fade drops it again, to the lowered volume of the one
  remembered. `08`/`09` with nothing lowered set nothing and ask BlueZ nothing.
- A `Set` of `Volume` changes the property only when the pods answer, to the volume they
  report (BlueZ 5.87, `transport_a2dp_src_set_volume`, `avrcp_handle_set_volume`; how
  long the pods take, and whether they round, is untried). So nothing waits for an
  answer or reads one back: a reading counts as ours if it is within
  `volume::READ_BACK_SLACK` (a guess) of any volume set this stretch, or is the volume
  before while the lowering is unanswered. Never compare with the last value set alone,
  and never take the volume before as the user's while the lowering is unanswered. The
  fake BlueZ answers late and rounded with `SetVolumeAnswer`;
  `docs/app/volume.md` says why, and what it costs.
- The user's change wins: put back, or a fade stepped, only while the transport reads a
  volume that is ours; the lowering itself sets from any volume. Moved meanwhile, the
  remembered volume is forgotten.
- Put back when the link goes - `SpeechEvent::LinkGone`, from `drop_link` under `inner`'s
  write lock after the link's last level - and at shutdown, `main.rs` waiting at most
  `volume::RESTORE_LIMIT`, on Quit or the first SIGTERM or SIGINT; a second exits at
  once. A transport gone meanwhile is waited for, as bluetoothd starts the next one at
  the device's volume: looked for each `volume::PUT_BACK_CHECK` for
  `volume::PUT_BACK_WINDOW` (guesses) from when it went, or from the pair's next link
  (`SpeechEvent::LinkUp`), and past that kept until the next link or level, and put
  back if it reads ours. Not in a pair's supervisor: that is aborted wherever it is.
- Levels reach it as events (`Coordinator::speech_events`), not snapshots: the watch
  keeps the latest alone, and a short stretch's `01` could be gone before a consumer
  looked. Every BlueZ call goes without auto-start, each call and the bus connection
  are bounded by `volume::CALL_LIMIT`, and the task takes no signal. Events go first in its `select!`,
  so `VolumeControl::settled` answers for every event told before it: tests wait on it,
  never on a sleep.

**The media paused when a pod leaves an ear is every MPRIS player's, from the link's ear
state alone.** The pods only report where they are; that they pause nothing themselves is
believed, not checked (#167). `media::EarPause` decides, pure and taking the time;
`media::Media` carries it out in a task of its own, on the session bus for the players
and the system bus for the transport. `docs/app/media-pause.md` has the table.
- The places reach it as events (`Coordinator::ear_events`, `podstate::EarEvent`), from
  the link's `EarState` once it has both packets, never from the advertisement or the
  snapshot: the snapshot falls back to the advertisement's in-ear and does not say so,
  and that lags and contradicts itself (#142). Told under `inner`'s write lock, before
  the snapshot that shows them, between the link's `LinkUp` and `LinkGone`. A link's
  first places pause nothing.
- A pod in an ear leaving it, with nothing paused, pauses every `Playing` player - only
  while an A2DP transport of the pair is `active` (`volume::a2dp_active_in`, with a
  `Volume` or not), so a call, assumed to leave A2DP not active, is left alone - and
  remembers the players and the pods worn just before, less any that refused `Pause`
  (one that timed out is kept: the pause may have landed). Every pod worn then back in an ear plays each remembered player still
  `Paused`, or still `Playing` within `media::PAUSE_SETTLE` (a guess) of the pause, and
  forgets the rest: the user's. The transport is not read for that. A pod going into the
  case (`02`/`03`), the link going and the setting turned off forget the pause; the
  switch reaches the task as a `media::Switch` that counts each turning off, so off and
  on again before the task looked forgets too.
- Explicit `Pause` and `Play`, never `PlayPause`: a toggle could start what the user
  paused. No key is synthesized: that needs `/dev/uinput`, and the shell picks a player.
- Players are `org.mpris.MediaPlayer2.*` names, called at their owner's unique name, one
  owner once. Every call goes without auto-start and within `media::CALL_LIMIT`, the
  players' side by side; the task reads only what a decision needs (`media::Needs`) and
  takes no signal, so the players are seen only when the pods are back: one played by
  hand reads `Playing` and is forgotten once `PAUSE_SETTLE` has passed, but one played and
  paused again by hand reads `Paused`, and plays. Tests give it a longer limit
  (`Media::with_call_limit`, harness only).
  Not in a pair's supervisor. Events go first in its `select!`, the setting next, so
  `MediaControl::settled` answers for every event told before it: tests wait on it.

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
reconnect it. The supervisor asks `Device1.Connected` before every attempt but the first -
`attach` starts it only on BlueZ's own word that the pair is connected, never a signal's,
read last - and asks nothing while BlueZ is off the bus: under systemd a call starts it
again.

**Never hold the `inner` lock across a read or a send on the socket.** `AapLink::run`
hands the link's client to the read loop and the command loop, and neither looks it up
under the lock. A read parks until a packet arrives; holding the lock there once
deadlocked every command sent meanwhile.

## Patterns

- Keep protocol parsing pure and tested. `aap::battery`, `aap::conversation`, `aap::ear`,
  `aap::info`, `aap::keys`, `aap::name`, `aap::noise`, `aap::message`, `ble::parser`, `ble::decrypt`, `ble::rpa` take bytes and
  return values, and carry the bulk of the test suite; `noise::ModeState`,
  `ear::EarState`, `rpa::ResolvedAddresses`, `notify::Alerts`, `volume::Ducking` and
  `media::EarPause` take the time as an argument for the same reason; `volume::a2dp_in`
  and `volume::a2dp_active_in` read BlueZ's objects as values. `aap::decode` routes every packet by its header and opcode;
  each module's `is_*` and `parse_*` still check the header themselves, so they stay safe
  to call on their own.
- Use synthetic test vectors - never commit a real device key. New test keys come from
  `ble::decrypt::random_key()`, not literals (CodeQL's hard-coded key rule, #66); the
  fixed keys in podstate's and keystore's tests predate that, and the Core spec's public
  RPA sample (Vol 3 Part H Appendix D.7) is marked `gitleaks:allow`. Encrypt in the test
  and assert the round trip, beside a check of the bytes against a value from outside the
  code: a round trip misses a mistake made the same way both ways, as the IRK's byte
  order would be.
- Test what a caller sees - the value returned, the packet sent, the snapshot broadcast,
  what the fake BlueZ or player holds - never words in docs, comments or source. A check
  of a contract something else reads stays: the GResource names the UI loads, a file's
  format.
- Expected values come from outside the code under test: a capture, the spec's sample, a
  value worked out by hand. An answer computed with the function it checks proves nothing.
- Assert the outcome, not only that a fake was called: a fake keeps the behavior apart,
  it is not the behavior.
- No tests of constants, derives or getters, or of behavior another test covers already.
  An assert that keeps a test's own premise true is not one.
- A refactor that changes no behavior breaks no test: pin private structure, call counts
  or ordering only where they are the behavior - commands in the order of the calls
  (#54), no write for an unchanged key (#38), no AAP attempt to a pair BlueZ calls
  disconnected.
- Make a slow test faster without dropping its cases. What must come is waited for on a
  condition - a `settled`, a first event seen - never on a sleep. What must not come is
  waited for with a `settled` where the task has one, else with a named sleep that says
  why it is long enough, as the pipeline's `QUIET` does.
- `Option<u8>` for unknown battery, never a sentinel. Levels above 100 mean unavailable,
  and so does an AAP part with status Disconnected (`0x04`), whatever its level byte: it
  comes as `0x00` as well as `0xFF`, and the `0x00` read as a real 0% put the case at 0%
  (#108).
- Log stages separately: `BLE parsable:` covers every advertisement that parses, including
  ones the coordinator later drops; `BLE decryptable:` reports identification. Noisy
  non-proximity Apple data goes to `trace`.
- Bytes in logs and probe output go through `linuxpods::hex`, and a packet that may carry
  keys or serial numbers through `aap::keys::redacted_hex`, which shows only the preamble
  of a key packet or a device information packet (`0x1D`). No log, probe or example
  prints key bytes or serial numbers: their output gets pasted into issues.
- Log text describing control flow goes stale like comments, and is more visible.
- Tasks on the system bus (`ble_task`, `attachment::run`) never return for good: they set
  themselves up again with `bluez::Backoff`, which counts a run under a minute as a
  failure, so a connection that dies at once is not retried in a loop. Each used to give
  up at its first error, and the app stayed deaf until restarted (#50). `volume::Volume`
  calls BlueZ only for a speech level, a fade step, a look for a transport that went, a
  link going or the quit, connecting when one needs it and again after a connection
  failed, so it has no retry loop to back off; `media::Media` likewise calls only for a
  pod leaving an ear or coming back.
- A harness build must behave like a plain one unless the fake world's environment asks
  otherwise (`LINUXPODS_AAP_SOCKET_DIR`, `LINUXPODS_HARNESS_AUTOMATION=1`, set only by
  `scripts/fake-world.sh`): `make test` and `make fake-world` leave it at
  `target/debug/linuxpods`, where it meets real AirPods and the user's session. Nothing in
  the harness writes to the app's real data or config directory, and its fake media
  player goes only on a private session bus: `fakepods` refuses `/run/user/<uid>/bus`.

**UI.** Widget updates arrive only from the snapshot stream. The one exception is a
command that fails: its result comes back to the main context and goes up as a toast, an
event, never a control's state, so the noise radios keep showing the recorded mode. A
setting is not device state: its switch is bound to an app action and shows that action's
state, as Battery notifications does with `app.battery-notifications` and pausing on the
ears with `app.pause-on-ear`. Read
a selection *before* mutating the widget - splicing a `StringList` makes `GtkDropDown`
emit `selected-notify`, clobbering the user's choice. Noise Control, Conversation
Awareness and the Info group's Name row are commands, so they stay insensitive unless
the device is on AAP - and the switch until the setting is reported, as a switch cannot
show unknown, and the Name row until the pods' name is known; BLE is receive-only. A
snapshot turns that switch under `ControlView::syncing_features`, so its `notify::active`
does not send the setting back out, and a failed command draws the last snapshot again,
which puts it back. The Name row shows the pods' own name where the link has it, else
BlueZ's alias, and the rename dialog starts from the pods' name, never the alias; it
refuses what `aap::name` would, and the name the pods have, and a failed rename is a
toast. The Firmware row below it is no command: it shows the version the pair last
reported this run, connected or not, in a selectable label, and "Unknown", insensitive,
before the pair's first link. Elsewhere - the switcher, a low battery notification - a
pair is named by `Snapshot::display_name`: an alias set in BlueZ, else the pods' name,
else the alias, then the model. BlueZ's alias keeps the old name until the pair connects
again, so named by it the switcher showed a rename only then (#160).
Low battery notifications are worked out on the tokio side (`notify::Alerts`) and sent
from the main context as `GNotification`s, one per pair. They warn once per crossing, never
per snapshot - the worst band stands until the part charges or clears
`notify::REARM_ABOVE`, so a reading flipping across a threshold sends nothing - and only
from a fresh reading - AAP, or BLE heard within `notify::FRESH`: the BLE cache outlives
a pair that has gone by a day (#126, #169). The Preferences switch is the app action
`app.battery-notifications`, read by the delivery loop on the main context, not the
notifier: off withdraws what is on show and on shows it again, while `notify::Alerts` goes
on tracking crossings. Its state is saved: `settings::open` loads the settings at startup,
before the action is created, so a switch saved off starts off and sends nothing, and
`settings::Saver` writes each change on tokio's blocking pool, never the main loop - the
latest settings, whatever order the saves run in. A file that does not parse is set aside
and the defaults used; a save that fails keeps the change in memory. The switch for
pausing the media when a pod leaves an ear is the app action `app.pause-on-ear`
(`Settings::pause_on_ear`, on by default, and for a file from before it), saved the same
way; each change reaches `media::Media` on a `watch`, read at every ear event, and off
forgets what was paused.
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

[`docs/README.md`](docs/README.md) says what goes where, and the rules for every
document: `docs/protocol/` holds what the AirPods and the Linux stack do, `docs/app/`
what LinuxPods does about it, and `docs/development/` what working on LinuxPods needs.

`docs/protocol/aap/`:
- [`session.md`](docs/protocol/aap/session.md) - L2CAP PSM 4097, the handshake, the
  0x4D features packet, the startup dump waiting for 0x0C, the packets around the keys
- [`keys.md`](docs/protocol/aap/keys.md) - the key request and key packet, the IRK and
  ENC_KEY, the IRK's byte order, keys kept across a re-pairing
- [`battery.md`](docs/protocol/aap/battery.md) - the battery packet (0x04), its codes,
  Disconnected's placeholder levels (#108), the primary pod listed first
- [`ear.md`](docs/protocol/aap/ear.md) - the ear report (0x06) and primary pod (0x08),
  the role switch, the captures
- [`settings.md`](docs/protocol/aap/settings.md) - the 0x09 family of settings
- [`noise-control.md`](docs/protocol/aap/noise-control.md) - the mode packet, 0x34
  before Off, what answers a change, stale reports, the runs
- [`conversation-awareness.md`](docs/protocol/aap/conversation-awareness.md) - the 0x28
  setting, the speech levels (0x4B), the pods' own switch to Transparency, the levels
  around a mode change, the hardware run
- [`device-info.md`](docs/protocol/aap/device-info.md) - the device information packet
  (0x1D): its layout and fields, the firmware version's layout as read off one value
- [`rename.md`](docs/protocol/aap/rename.md) - the rename packet (0x1A), the pods' name,
  the hardware and name runs, 127 bytes

`docs/protocol/ble/`:
- [`proximity-pairing.md`](docs/protocol/ble/proximity-pairing.md) - the advertisement's
  bytes, decryption, the decrypted layout, suffix validation, update frequency
- [`addresses.md`](docs/protocol/ble/addresses.md) - resolvable private addresses, their
  rotation, resolving them with the IRK, its byte order

`docs/protocol/linux/`:
- [`l2cap.md`](docs/protocol/linux/l2cap.md) - a connect that returns with the channel
  still pending (#136)
- [`bluez.md`](docs/protocol/linux/bluez.md) - discovery and device cleanup, adapters
  and restarts, starting bluetoothd, battery providers, `MediaTransport1.Volume`,
  `Device1.Name` and `Device1.Alias`
- [`zbus.md`](docs/protocol/linux/zbus.md) - the queue that stops the connection, the
  object server's late dispatch, unchecked well-known senders, order across
  subscriptions
- [`desktop.md`](docs/protocol/linux/desktop.md) - PipeWire following the transport's
  volume, MPRIS, the path a press on a pod takes to a player, UPower's warning level

`docs/app/`:
- [`identification.md`](docs/app/identification.md) - identification by IRK then
  ENC_KEY, why both, what is dropped
- [`keystore.md`](docs/app/keystore.md) - keys requested on every connection, where they
  are kept, the IRK kept as sent and reversed to resolve
- [`aap-links.md`](docs/app/aap-links.md) - waiting for the connect and the handshake,
  the features packet sent on every link
- [`state.md`](docs/app/state.md) - the lid kept across the pods' advertisements
- [`readings.md`](docs/app/readings.md) - the lid left out of the readings kept across a
  relaunch
- [`in-ear.md`](docs/app/in-ear.md) - in-ear from AAP, the role switch held back
- [`noise-control.md`](docs/app/noise-control.md) - 0x34 only for Off, the mode recorded
  optimistically, stale reports held off
- [`conversation-awareness.md`](docs/app/conversation-awareness.md) - the setting
  recorded optimistically, the levels as events
- [`volume.md`](docs/app/volume.md) - the volume lowered while the user speaks: what is
  ours, the fade, putting it back, A2DP active only
- [`media-pause.md`](docs/app/media-pause.md) - the media paused when a pod leaves an
  ear and played again once it is back
- [`names.md`](docs/app/names.md) - the pods' name beside BlueZ's alias, the Name row,
  the rename dialog
- [`firmware.md`](docs/app/firmware.md) - the firmware version read, kept for the run,
  and shown
- [`logging.md`](docs/app/logging.md) - what the packet log lines show, and what is never
  logged of the device information packet
- [`ui.md`](docs/app/ui.md) - the pair's color on the Control page

`docs/development/`:
- [`releasing.md`](docs/development/releasing.md) - release steps, what a release
  contains, verifying one
- [`fake-world.md`](docs/development/fake-world.md) - what the fake pods, BlueZ and
  player do, and where they depart from the hardware
- [`probes.md`](docs/development/probes.md) - the key and decryption probes, what they
  print, troubleshooting
