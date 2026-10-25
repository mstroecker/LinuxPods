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
├── aap/           # Apple Accessory Protocol over L2CAP (PSM 4097)
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
  pair. It runs *after* decryption, since an advertisement carries only a random MAC
  until then.
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
- Every connected pair has its own AAP link (`Inner::links`): client, reading, and a
  read loop `connect_aap` starts with that client. The loop never looks its client up
  by MAC - a loop outliving its link would read the replacement's socket under the old
  identity, which once stored one device's key under another's MAC. Commands
  (`set_noise_control`, `request_encryption_keys`, `disconnect_aap`) name their device,
  and GNOME Settings gets one `BatteryProvider1` object per attached pair.
- Both `connect_aap` and `disconnect_aap` broadcast. Disconnect also drops the device's
  AAP state, or the UI keeps showing a stale exact-looking reading.
- BLE is cached, AAP never is. `Inner::ble` keeps the last advertisement per device -
  also the connected one's, underneath AAP - so disconnect falls back to it at once.
  AAP lives in the device's link and goes with it. Identified readings expire after
  `BLE_CACHE_TTL` (30 min) and carry `last_seen` for the UI; `expiry_task` prunes on a
  clock, since with no device in range no advertisement ever triggers a prune.
- Apple rotates the advertised BLE MAC every few seconds while disconnected, so an
  advertisement no stored key decrypts cannot be attributed to anything. It is dropped on
  arrival - not stored, not broadcast; storing those is what a BLE spam flood fed on.
  The scanner's `BLE parsable:` log still records it.
- A D-Bus subscription that advertisements reach is drained by a task that never awaits
  its reader, and what an advertiser can trigger at will is coalesced per device - never
  queued per signal, never dropped. Left in zbus's 64-message queue, advertisements
  stopped zbus reading the connection, and a reply attachment awaited never came (#90).
  Queued one per signal, an advertiser re-signalling its services kept a pair waiting
  55 s to attach in a test (#96). `Connected` changes are kept, in order.
- Adapters are found, never assumed: `bluez::adapters` lists them from
  `GetManagedObjects`. Discovery runs on every powered one, subscribed first, since an
  address's first advertisement comes in the `InterfacesAdded` that announces it. Each
  adapter has a provider root of its own, registered with its own
  `BatteryProviderManager1`, and a pair's battery goes to the provider its
  `Device1.Adapter` names: bluetoothd ignores a battery whose device is on another
  adapter. With `/org/bluez/hci0` hardcoded, a machine whose adapter was another heard
  nothing over BLE (#51).

## Protocol

**Keys are requested on every AAP connection**, not only when none is stored. Re-pairing
regenerates them, and a stale key fails suffix validation without a word, dropping BLE
to 10% steps. The request is one packet; `aap_read_loop` stores the key that comes back if it is
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

**Never hold the `inner` lock across `read_packet().await`** - clone the link's client out. The read parks until a
packet arrives; this deadlocked the Request Keys button.

## Patterns

- Keep protocol parsing pure and tested. `aap::battery`, `aap::keys`, `ble::parser`,
  `ble::decrypt` take bytes and return values, and carry the bulk of the test suite.
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
