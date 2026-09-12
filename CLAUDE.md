# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## Project

LinuxPods manages Apple AirPods on Linux: a GTK4/libadwaita GNOME app with battery
monitoring, a system tray and GNOME Settings integration. Rust, edition 2024, 1.85+.
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
├── bluez.rs       # org.bluez.BatteryProvider1 + device connection watch
├── keystore.rs    # Encryption keys at ~/.local/share/linuxpods/keys.json (0600)
├── indicator.rs   # System tray via StatusNotifierItem (ksni)
└── ui.rs          # GTK4/libadwaita interface
```

**Threading.** GTK owns the main thread, tokio carries BLE/AAP/D-Bus. GTK types are
`!Send`, so the two meet over `async-channel` consumed with `glib::spawn_future_local`.
Never touch a widget from a tokio task.

**State.** `Coordinator` merges AAP (exact, needs L2CAP) and BLE (10% steps, or exact
once a stored key decrypts) **per device**, keyed by the **real** MAC.

- `supersedes_ble` is per-device, never global - a global pause blanks out every other
  pair. It runs *after* decryption, since an advertisement carries only a random MAC
  until then.
- `subscribe()` returns a channel per consumer and delivers the current state at once. A
  single shared `async_channel::Receiver` is MPMC, so UI, tray and provider would compete
  for each snapshot instead of all seeing it; without the immediate delivery the window
  comes up blank until the first advertisement.
- Both `connect_aap` and `disconnect_aap` broadcast. Disconnect also drops the device's
  AAP state, or the UI keeps showing a stale exact-looking reading.
- BLE is cached, AAP never is. `Inner::ble` keeps the last advertisement per device -
  also the connected one's, underneath AAP - so disconnect falls back to it at once.
  AAP lives in its own slot tied to `connected_mac`. Identified readings expire after
  `BLE_CACHE_TTL` (30 min) and carry `last_seen` for the UI; `expiry_task` prunes on a
  clock, since with no device in range no advertisement ever triggers a prune.
- Apple rotates the advertised BLE MAC every few seconds while disconnected. Devices that
  cannot be identified still need `DEVICE_TTL` pruning; their addresses never collapse.

## Protocol

**BLE validation is by MAC suffix.** Bytes 7-9 of the decrypted payload hold the last
three bytes of the real MAC; a match both validates and identifies. A connected device
zeroes the field, so `00 00 00` is accepted too - identification comes from *which key*
decrypted, not from reading the suffix, and the union costs one bit (2^-23 per wrong key).

⚠️ Do **not** validate with magic bytes. The older check (byte 0 upper nibble `0x0`, byte
4 `0x2D`) holds on neither AirPods Pro 3 (0x2720) nor Pro Gen 2 (0x2420) - both report
byte 4 = `0x1D` - and rejects correct decryptions, silently disabling 1% accuracy.

**AAP packets carry battery only.** Model, colour and orientation carry forward from BLE
state for the same device. The noise control mode is *not* carried forward - it lives in
its own map on the coordinator, because it arrives on its own schedule and the device
entry may not exist yet when the startup dump reports it.

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

**A zero `cid` immediately after `SeqPacket::connect` means the socket is unusable** -
connect returns `Ok` without waiting for the BR/EDR ACL link, every send then fails
`ENOTCONN`, and the socket stays dead even once `cid` becomes nonzero. Discard and retry.

**Never hold the `aap_client` mutex across `read_packet().await`.** The read parks until a
packet arrives; this deadlocked the Request Keys button.

## Patterns

- Keep protocol parsing pure and tested. `aap::battery`, `aap::keys`, `ble::parser`,
  `ble::decrypt` take bytes and return values, and carry the bulk of the test suite.
- Use synthetic test vectors - never commit a real device key. Encrypt in the test and
  assert the round trip.
- `Option<u8>` for unknown battery, never a sentinel. Levels above 100 mean unavailable.
- Log stages separately: `BLE parsable:` covers every advertisement that parses, including
  ones the coordinator later drops; `BLE decryptable:` reports identification. Noisy
  non-proximity Apple data goes to `trace`.
- Log text describing control flow goes stale like comments, and is more visible.

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
