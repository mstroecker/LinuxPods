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

Each rule below is short; the link after it goes to the document that says why, with the bugs
behind it.
How the parts fit together: [docs/app/architecture.md](docs/app/architecture.md).

**Threading.** [Why](docs/app/architecture.md#two-threads)
- GTK owns the main thread, tokio carries BLE/AAP/D-Bus; GTK types are `!Send`, so the two meet over
  channels (`watch`, `async-channel`, `oneshot`), consumed with `glib::spawn_future_local`.
- Never touch a widget from a tokio task.

**State.** [Why](docs/app/state.md)
- `Coordinator` merges AAP and BLE **per device**, keyed by the **real** MAC.
- `supersedes_ble` is per-device, never global, holds only once the link has a reading, and runs
  after identification (#62). [Why](docs/app/state.md#per-pair)
- `subscribe()` returns a `watch::Receiver`, latest-wins and marked changed; never a queue per
  consumer. [Why](docs/app/state.md#snapshots)
- Never hold `borrow()` across an await: every broadcast waits for it under `inner`'s write lock.
  The UI loop pauses `RENDER_INTERVAL` per pass. [Why](docs/app/state.md#snapshots)
- Snapshots go out under the lock they were changed under: `broadcast(&inner)` while the write guard
  is live, never a snapshot built earlier (#88). [Why](docs/app/state.md#snapshots)
- Both `open_aap` and `disconnect_aap` broadcast; disconnect drops the pair's AAP state, all but the
  firmware version. [Why](docs/app/state.md#what-a-snapshot-shows)
- BLE is cached (`Inner::ble`, the connected pair's too), AAP never is but the firmware version
  (`Inner::firmware_versions`, never pruned, the next link's replaces it, #161). Readings expire
  after `BLE_CACHE_TTL` (#169), pruned by `expiry_task` on a clock.
  [Why](docs/app/state.md#the-ble-cache)
- The BLE cache's ages are on the wall clock (`SystemTime`, `podstate::age`), never `Instant`. A
  reading heard after now - the clock set back - has no age: it goes at the next prune and on load,
  and is never fresh for a notification. Every other `Instant` (AAP, ear, noise, volume, media)
  stays one. [Why](docs/app/state.md#the-wall-clock)
- One advertisement never wholly replaces the lid: with status bit 4 set the cached lid is kept. The
  parser stays stateless. [Why](docs/app/state.md#the-lid)
- `readings.json` keeps levels, charging flags, model and color and heard time only - not in-ear,
  the lid or the connection state - beside the keys, mode 0600, written to a temp file and renamed,
  as the keystore. It loads only for a pair with keys heard within `BLE_CACHE_TTL`; a file that does
  not parse is set aside, and a level above 100 loads as unknown. A restored reading
  (`PodState::restored`) is left out of GNOME Settings by that flag, never by a freshness window
  (#62, #170). [Why](docs/app/readings.md)
- Readings are never saved per advertisement: only when `readings::due` says so, and once at the
  quit. [Why](docs/app/readings.md#saving)
- The readings and keystore locks are taken before `inner`, never under it: each save holds its
  lock across the write and takes `inner` under it. Each save moves its owned lock into
  `spawn_blocking`, so an abort never leaves it half done. [Why](docs/app/readings.md#saving)
- An advertisement counts only if a stored IRK resolves its address and that pair's ENC_KEY decrypts
  it; only that key is tried (#134). Anything else is dropped on arrival: not stored, not broadcast.
  [Why](docs/app/identification.md#why-both-checks), [and why dropped](docs/app/identification.md#keyed-by-the-real-mac)
- `rpa::ResolvedAddresses` is bounded and expiring, holds only addresses that resolved, each with
  the IRK that resolved it, and is a std mutex beside `inner`, taken alone and never held across an
  await. [Why](docs/app/identification.md#resolved-addresses)
- A pair with no IRK yet is identified by its ENC_KEY alone until its next AAP connection stores
  one. [Why](docs/app/identification.md#pairs-with-no-irk)
- Speech-event and ear-event consumers are a std mutex each beside `inner`: taken under its write
  lock to tell an event, alone to register, never across an await. [Why](docs/app/state.md#events)

**AAP links and commands.** [Why](docs/app/aap-links.md)
- Every attached pair has a supervisor (`attachment::supervise`); attachment's event loop only
  starts and stops supervisors (#49). [Why](docs/app/aap-links.md#the-supervisor)
- `stop` aborts the supervisor and awaits its `JoinHandle` before `disconnect_aap`. Nothing a
  supervisor awaits may leave work half done. [Why](docs/app/aap-links.md#the-supervisor)
- `open_aap` refuses a device that has a link, before and after `aap::Client::open`, and records the
  link and its queue as its last step, with nothing awaited after.
  [Why](docs/app/aap-links.md#opening-a-link)
- A dropped `AapLink` - timed out, aborted, run to its end - shuts the socket and removes its own
  record and queue, matched by client, so a newer link stands.
  [Why](docs/app/aap-links.md#running-and-ending)
- A read loop reads its own client, never one looked up by MAC; what a loop or a command writes goes
  through `Inner::live_link`. [Why](docs/app/aap-links.md#running-and-ending)
- Commands take one path, `Coordinator::command(mac, Command)`, queued in the call itself, not at
  the first poll (#54). [Why](docs/app/commands.md#one-path)
- The queues are a std mutex beside `inner`: taken only under its write lock or alone, never across
  an await, changed only with `Inner::links`, removed only past `drop_link`'s client check.
  [Why](docs/app/commands.md#the-queues)
- Each caller gets a `Result`, `ConnectionLost` when the link went mid-command; dropping the future
  does not withdraw it. The window toasts a failure, the tray logs it.
  [Why](docs/app/commands.md#what-the-caller-hears)

**BlueZ and D-Bus.** [Why](docs/app/bluez.md)
- A D-Bus subscription that advertisements reach is drained by a task that never awaits its reader;
  what can be triggered at will is coalesced per device or adapter, never queued per signal, never
  dropped; `Connected` changes are kept in order (#90, #96).
  [Why](docs/app/bluez.md#subscriptions-that-advertisements-reach)
- A draining task never calls BlueZ.
  [Why](docs/app/bluez.md#subscriptions-that-advertisements-reach)
- A zbus connection that serves objects is built with one served already (`serve_at`), never plain:
  `bluez::connection`, `harness::connect_serving` (#125).
  [Why](docs/app/bluez.md#connections-that-serve-objects)
- A BlueZ signal counts only from the unique name the state was read from, and `NameOwnerChanged`
  only from `org.freedesktop.DBus` (#50). [Why](docs/app/bluez.md#which-signals-count)
- A `Connected` change is only a hint: act only if `Device1.Connected` agrees, no answer reading as
  disconnected; `attach` reads it once more, last (#132).
  [Why](docs/app/bluez.md#connected-is-a-hint)
- Nothing starts bluetoothd: no call while BlueZ is off the bus, `NoAutoStart` before it is known,
  no call for an event BlueZ did not confirm (#132).
  [Why](docs/app/bluez.md#never-starting-bluetoothd)
- Signals whose order matters come from one match rule, one stream.
  [Why](docs/app/bluez.md#signals-in-order)
- Adapters are found, never assumed (`bluez::watch_adapters`, #50, #51); discovery runs on every
  powered one, subscribed first (#153). [Why](docs/app/bluez.md#adapters)
- Consumers start again when the `AdapterSet` token changes, never on a single signal;
  `AlreadyExists` counts as registered (#50). [Why](docs/app/bluez.md#adapters)
- A provider root per adapter per connection, named after the unique name; a pair's battery goes to
  the provider its `Device1.Adapter` names (#51). [Why](docs/app/bluez.md#battery-providers)
- GNOME Settings gets a battery only for an attached pair with a reading: absent, never 0%, when the
  level is unknown. [Why](docs/app/battery.md#gnome-settings)
- `InProgress` from `StartDiscovery` never counts as started: stop, start again, and wait for the
  backoff at a second refusal (#123). [Why](docs/app/bluez.md#discovery)

## Protocol

**Say where a reading comes from.** A protocol reading taken from LibrePods, or guessed,
says so where it is written - doc, comment, example, README - until a run here confirms
it, and names that run once one does. The magic-byte check and the `0x4B`
"settings-changed notification" were each a guess written down as fact, and each stood
until hardware showed otherwise: the check rejected every correct decryption, and the
notification was Conversation Awareness's speech level `09` (#150).

**The session.** [Why](docs/app/aap-links.md)
- Nothing goes out before the handshake is answered: `Client::handshake` returns once the answer is
  in. [Why](docs/app/aap-links.md#the-handshake)
- A zero `cid` after an L2CAP connect means the connect has not finished: never drop a socket while
  it is pending (#136). `Client::connect` waits in a task of its own, up to `CONNECT_LIMIT`;
  `AAP_CONNECT_TIMEOUT` outlasts connect and handshake. [Why](docs/app/aap-links.md#connecting)
- AAP only to a pair BlueZ reports connected: an L2CAP connect pages the device. The supervisor asks
  `Device1.Connected` before every attempt but the first. [Why](docs/app/aap-links.md#which-pairs)
- Never hold the `inner` lock across a read or a send on the socket.
  [Why](docs/app/aap-links.md#running-and-ending)

**Keys and identification.** [Why](docs/app/keystore.md)
- Keys are requested on every AAP connection, by `open_aap` right after `aap::Client::open`, never
  by `open`. [Why](docs/app/keystore.md#requested-on-every-connection)
- A key is taken only if it is 16 bytes; a packet without a usable IRK keeps the stored one while
  the ENC_KEY is unchanged, and a new ENC_KEY drops it.
  [Why](docs/app/keystore.md#which-keys-are-taken)
- The keystore is written only when the keys differ from those last saved, so a save that failed
  is retried (#38). [Why](docs/app/keystore.md#written-only-when-changed)
- Format version 2 keeps the IRKs in an `irks` map beside `keys`, so a version 1 file loads
  unchanged and a version 1 reader still finds its keys. [Why](docs/app/keystore.md#the-file)
- The key packet carries the IRK byte-reversed: the keystore keeps it as sent, `ble::rpa::resolves`
  reverses it. [Why](docs/app/keystore.md#the-file)
- The suffix check accepts the pair's MAC suffix or `00 00 00` (#71).
  [Why](docs/app/identification.md#the-suffix)
- ⚠️ Do **not** validate with magic bytes (byte 0 upper nibble `0x0`, byte 4 `0x2D`): they reject
  correct decryptions on a Pro 3 and a Pro 2 (USB-C). [Why](docs/app/identification.md#the-suffix)

**In-ear.** [Why](docs/app/in-ear.md)
- In-ear comes from the link's `EarState` once it has both `0x06` and `0x08`, else from the cached
  advertisement. Never take the primary pod from the advertisement (#71, #142).
- `EarState` holds a role switch's `0x06` for the `0x08`, at most `ROLE_SWITCH_HOLD`.
  [Why](docs/app/in-ear.md#the-role-switch)
- A snapshot shows no primary pod (#152). [Why](docs/app/state.md#what-a-snapshot-shows)

**Noise control.** [Why](docs/app/noise-control.md)
- `set_noise_control` sends `0x34 01` immediately before Off, and only for Off.
  [Why](docs/app/noise-control.md#allowing-off)
- The mode is recorded optimistically on the link (`Link::noise_mode`), never waited for, kept apart
  from the reading every battery packet replaces, and goes with the link. [Why](docs/app/noise-control.md#recorded-not-confirmed)
- Validate a `0x09` report on the `04 00 04 00` header, byte 4 **and** byte 6.
  [Why](docs/app/noise-control.md#recorded-not-confirmed)
- A report of the mode the last command replaced is held off (`aap::noise::ModeState`, on the
  link) until the mode set is reported, or `STALE_REPORT_WINDOW` has passed since the command was
  recorded; a held report is dropped, not replayed; past the window any report wins. Only the last
  command's replaced mode is held off (#113).
  [Why](docs/app/noise-control.md#stale-reports)

**Conversation Awareness.** [Why](docs/app/conversation-awareness.md)
- The setting (`0x28`) is recorded optimistically on the link and goes with it.
  [Why](docs/app/conversation-awareness.md#the-setting)
- A speech level (`0x4B`) is read only with bytes 5-8 checked, as a target, not a step; `08` and
  `09` acknowledge nothing; every level goes to the volume as an event.
  [Why](docs/app/conversation-awareness.md#the-speech-levels)

**Rename and device information.** [Why](docs/app/names.md)
- Rename is fire and forget, recorded on the link, and never touches BlueZ's alias: `Device1.Alias`
  is never set or cleared (#160). [Why](docs/app/names.md#two-names)
- A name that is empty, has a NUL or runs over 127 bytes is refused, never sent and never cut by
  bytes - by the dialog, by `Coordinator::command`, and by `aap::name`.
  [Why](docs/app/names.md#rename)
- `0x1D` is read only with `02` at byte 6 and `04 00` at bytes 9-10; only the name and the firmware
  version are read or logged, and `aap::keys::redacted_hex` cuts it.
  [Why](docs/app/logging.md#device-information)
- The firmware version is field 10, read only through `aap::info::FirmwareVersion`, its value logged
  beside the reading; each field fails on its own, and an error never carries the field's bytes.
  [Why](docs/app/firmware.md)

**The volume lowered while the user speaks.** [Why](docs/app/volume.md)
- The pair's own `org.bluez.MediaTransport1.Volume`: never the default output, never PipeWire, never
  a volume in software. [Why](docs/app/volume.md#lowering-the-volume)
- The transport is looked up under the pair's device path by `Device` and `UUID` every time, never
  kept. [Why](docs/app/volume.md#lowering-the-volume)
- Lowered only while that A2DP transport is `active`. [Why](docs/app/volume.md#only-while-a2dp-plays-never-in-a-call)
- Down at once to `volume::LOWERED_PERCENT` at the first `01`/`02`; back over `volume::FADE` from
  the first level of `03` or above; `08`/`09` with nothing lowered set nothing and ask BlueZ
  nothing. [Why](docs/app/volume.md#lowering-the-volume)
- A reading is ours if it is within `volume::READ_BACK_SLACK` of any volume set this stretch, or is
  the volume before while the lowering is unanswered. Never wait for the pods' answer or read one
  back, never compare with the last value set alone, and never take the volume before as the
  user's while the lowering is unanswered. [Why](docs/app/volume.md#what-is-ours)
- The user's change wins: put back, or a fade stepped, only while the transport reads a volume that
  is ours; the lowering itself sets from any volume. Moved meanwhile, the remembered volume is
  forgotten. [Why](docs/app/volume.md#the-users-change-wins)
- Put back on `SpeechEvent::LinkGone`, at the quit within `volume::RESTORE_LIMIT`, and once a
  transport that went is back; never in a pair's supervisor.
  [Why](docs/app/volume.md#put-back-later)
- Levels come as events; every BlueZ call goes without auto-start, and each call and the bus
  connection are bounded by `volume::CALL_LIMIT`;
  the task takes no signal; events go first in its `select!`, and tests wait on
  `VolumeControl::settled`, never on a sleep. [Why](docs/app/volume.md#the-task)

**The media paused when a pod leaves an ear.** [Why](docs/app/media-pause.md)
- From the link's ear events alone (`Coordinator::ear_events`), never the advertisement or the
  snapshot; told under `inner`'s write lock, before the snapshot. A link's first places pause
  nothing.
  [Why](docs/app/media-pause.md#from-the-link-alone)
- A pod in an ear leaving it, with nothing paused, pauses every `Playing` player, only while the
  pair's A2DP transport is `active` (`volume::a2dp_active_in`: no `Volume` needed), and remembers
  the players and the pods worn just before. A player that refused `Pause` is not remembered; one
  that timed out is, as the pause may have landed.
- Once every pod worn at the pause is back in an ear, play each remembered player still `Paused`,
  or still `Playing` within `media::PAUSE_SETTLE` of the pause, and forget the rest. A pod into the
  case (`02`/`03`), the link going and the switch off forget the pause - the switch a
  `media::Switch` counting each turning off.
  [Why](docs/app/media-pause.md#what-pauses-and-what-plays)
- Explicit `Pause` and `Play`, never `PlayPause`, and no key synthesized.
  [Why](docs/app/media-pause.md#explicit-pause-and-play-never-playpause)
- Players are `org.mpris.MediaPlayer2.*` names, called at their owner's unique name, one owner
  once, side by side, without auto-start, within
  `media::CALL_LIMIT`; only what `media::Needs` asks is read; no signal, not in a supervisor; events
  first in its `select!`, the switch next; tests wait on `MediaControl::settled`.
  [Why](docs/app/media-pause.md#the-task)

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
- A comment links a protocol fact rather than restating it, keeps the status its document
  gives it, tells no history, and claims only what the code beside it makes true; a
  test's doc says what the test asserts.
  [Why](docs/README.md#code-comments)
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

**UI.** [Why](docs/app/ui.md)
- Widget updates arrive only from the snapshot stream. The one exception is a command that fails: a
  toast, an event, never a control's state. [Why](docs/app/ui.md#updates-come-from-snapshots)
- A setting is not device state: its switch is bound to an app action (`app.battery-notifications`,
  `app.pause-on-ear`) and shows that action's state. [Why](docs/app/ui.md#settings)
- Settings load (`settings::open`) before any action is created, so a switch saved off starts off.
  `settings::Saver` writes each change on tokio's blocking pool, never the main loop: the latest
  settings, whatever order the saves run in. A file that does not parse is set aside and the
  defaults used; a save that fails keeps the change in memory. `app.pause-on-ear` is on by default,
  also for a file from before it. [Why](docs/app/ui.md#settings)
- Read a selection *before* mutating the widget: splicing a `StringList` makes `GtkDropDown` emit
  `selected-notify`, clobbering the user's choice. [Why](docs/app/ui.md#drop-downs)
- Noise Control, Conversation Awareness and the Name row stay insensitive unless the device is on
  AAP - the switch until the setting is reported, the Name row until the pods' name is known. A
  snapshot turns the switch under `ControlView::syncing_features`, and a failed command draws the
  last snapshot again, which puts the switch back.
  [Why](docs/app/ui.md#controls-that-send-commands)
- The Name row shows the pods' own name where the link has it, else BlueZ's alias; the rename dialog
  starts from the pods' name, never the alias, and refuses what `aap::name` would and the name the
  pods have. [Why](docs/app/names.md#the-name-row)
- The Firmware row is no command: the version last reported this run, else "Unknown", insensitive.
  [Why](docs/app/firmware.md#what-the-app-does-with-it)
- Elsewhere a pair is named by `Snapshot::display_name`: an alias set in BlueZ, else the pods' name,
  else the alias, then the model (#160). [Why](docs/app/names.md#everywhere-else)
- Low battery notifications are worked out by `notify::Alerts` on the tokio side and sent from the
  main context, one per pair, once per crossing, never per snapshot: the worst band stands until
  the part charges or clears `notify::REARM_ABOVE`. Only from a fresh reading: AAP, or BLE heard
  within `notify::FRESH` (#126, #169).
- The switch (`app.battery-notifications`) is read by the delivery loop, not the notifier: off
  withdraws what is on show, on shows it again, and `notify::Alerts` goes on tracking crossings.
  [Why](docs/app/notifications.md)
- A missing GResource fails silently: every name the UI loads is in
  `assets/resources.gresource.xml`, and a test asserts it. The tray points the shell at
  `assets/icons`. [Why](docs/app/ui.md#resources)

## Commits and pull requests

Do **not** put the Claude session URL in commit messages or pull request descriptions -
neither a `Claude-Session:` trailer nor a bare `https://claude.ai/code/session_...` link.
The links resolve for nobody but the author, and they outlive the session they point at.
A `Co-Authored-By:` trailer is fine.

## Documentation

[`docs/README.md`](docs/README.md) says what goes where, and the rules for every document.

[`docs/protocol/aap/`](docs/protocol/aap/) - what the AirPods do over AAP:
- [`session.md`](docs/protocol/aap/session.md) - the channel, the handshake, the requests, the startup dump, the opcodes
- [`keys.md`](docs/protocol/aap/keys.md) - the key request and packet, the IRK and ENC_KEY, keys kept across a re-pairing
- [`battery.md`](docs/protocol/aap/battery.md) - the battery packet (`0x04`), Disconnected's placeholder levels (#108)
- [`ear.md`](docs/protocol/aap/ear.md) - the ear report (`0x06`), the primary pod (`0x08`), the role switch
- [`settings.md`](docs/protocol/aap/settings.md) - the `0x09` family of settings
- [`noise-control.md`](docs/protocol/aap/noise-control.md) - the mode packet, `0x34` before Off, what answers a change
- [`conversation-awareness.md`](docs/protocol/aap/conversation-awareness.md) - the `0x28` setting, the speech levels (`0x4B`)
- [`device-info.md`](docs/protocol/aap/device-info.md) - the device information packet (`0x1D`), the firmware version
- [`rename.md`](docs/protocol/aap/rename.md) - the rename packet (`0x1A`), the pods' name, 127 bytes

[`docs/protocol/ble/`](docs/protocol/ble/) - the advertisements:
- [`proximity-pairing.md`](docs/protocol/ble/proximity-pairing.md) - the payload, decryption, the suffix
- [`addresses.md`](docs/protocol/ble/addresses.md) - resolvable private addresses, resolving them with the IRK

[`docs/protocol/linux/`](docs/protocol/linux/) - the stack under the app:
- [`l2cap.md`](docs/protocol/linux/l2cap.md) - a connect that returns with the channel still pending (#136)
- [`bluez.md`](docs/protocol/linux/bluez.md) - discovery and what it blocks, advertisement monitors, adapters and restarts, providers, `MediaTransport1.Volume`, names
- [`zbus.md`](docs/protocol/linux/zbus.md) - queues, the object server's dispatch, senders, order across subscriptions
- [`desktop.md`](docs/protocol/linux/desktop.md) - PipeWire, MPRIS, a press on a pod, UPower

[`docs/app/`](docs/app/) - what LinuxPods does, and why:
- [`architecture.md`](docs/app/architecture.md) - threads, channels, tasks, the coordinator; a map of the rest
- [`state.md`](docs/app/state.md) - the merge per pair, snapshots, events, the BLE cache and its clock, the lid
- [`identification.md`](docs/app/identification.md) - which advertisements count: IRK, then ENC_KEY, the suffix
- [`keystore.md`](docs/app/keystore.md) - keys requested on every connection, which are taken, `keys.json`
- [`readings.md`](docs/app/readings.md) - the last readings across a relaunch (#170)
- [`aap-links.md`](docs/app/aap-links.md) - which pairs get AAP, the supervisor, opening and ending a link
- [`commands.md`](docs/app/commands.md) - the one path for commands, their order, what the caller hears
- [`bluez.md`](docs/app/bluez.md) - adapters, discovery, providers, subscriptions, which signals count
- [`battery.md`](docs/app/battery.md) - unknown levels, where a level goes, GNOME Settings
- [`in-ear.md`](docs/app/in-ear.md) - in-ear from AAP, the role switch held back
- [`noise-control.md`](docs/app/noise-control.md) - `0x34` only for Off, the mode recorded, stale reports held off
- [`conversation-awareness.md`](docs/app/conversation-awareness.md) - the setting recorded, the levels as events
- [`volume.md`](docs/app/volume.md) - the volume lowered while the user speaks
- [`media-pause.md`](docs/app/media-pause.md) - the media paused when a pod leaves an ear
- [`names.md`](docs/app/names.md) - the pods' name beside BlueZ's alias, the rename dialog
- [`firmware.md`](docs/app/firmware.md) - the firmware version read, kept for the run, shown
- [`notifications.md`](docs/app/notifications.md) - low battery notifications
- [`ui.md`](docs/app/ui.md) - updates from snapshots, controls, settings, resources, the tray
- [`logging.md`](docs/app/logging.md) - log stages, what is never logged

[`docs/development/`](docs/development/) - working on LinuxPods:
- [`releasing.md`](docs/development/releasing.md) - release steps, what a release contains, verifying one
- [`fake-world.md`](docs/development/fake-world.md) - what the fakes do, and where they depart from the hardware
- [`probes.md`](docs/development/probes.md) - every probe in `examples/`: what it does, prints and never prints
