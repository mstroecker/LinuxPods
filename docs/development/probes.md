# Probes

The programs in `examples/` that the protocol runs were made with: what each does, how
to run it, what it prints, and what it never prints. Each example's doc comment is the
reference, and CI compiles every one; where this page and a doc comment disagree, the
doc comment is right and this page is to be fixed.

**Byte offsets** count from 0: in an AAP packet from its first byte, in a BLE payload
as [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md) counts them.

## Before a run

- **The pair must be connected** to this machine, for every probe that takes a MAC.
  The probe opens AAP to it, as the app does.
- **Quit LinuxPods first** for `irk_probe`, `rename_probe` and
  `conversation_awareness --duck`, so the probe's AAP session is the only one, or the
  only thing lowering the volume. The other probes say nothing either way.
- **The session is the app's** (`aap::Client::open`), the features packet included and
  the key request left out. Only `key_request` and `irk_probe` ask for keys. How the app
  opens a link: [app/aap-links.md](../app/aap-links.md#opening-a-link).

## What the output shows

Probe output is meant to be pasted into issues.

- **Never key bytes or serial numbers.** Packets go through `aap::keys::redacted_hex`,
  which cuts a key packet and a device information packet to its preamble:
  [app/logging.md](../app/logging.md#never-keys-or-serial-numbers).
- **The pair's address, in most probes.** Every probe that takes a MAC but `irk_probe`
  prints it back, and so do their errors. The library's log lines name it at `debug`,
  which only `noise_probe` logs at by default, and in its one warning, for a connect
  still pending after `aap::client::CONNECT_LIMIT`. `apple_sniff` prints random BLE
  addresses, and `decrypt_probe` the stored pairs' MACs. Only `irk_probe` prints none
  at its default log level ([`irk_probe`](#irk_probe)).
- **Host addresses, where a probe prints packets whole.** `noise_probe` prints every
  packet but a key packet and a device information packet whole, and `rename_probe`
  does the same after a rename and in each `--sweep` step. So they print `0x2B`, the
  hosts the pair knows (a phone among them), and `0x0C`, `0x0E` and `0x2E`, which name
  this host or a pod ([protocol/aap/session.md](../protocol/aap/session.md#opcodes)).
- **Names,** where a probe reads them: the pods' name from the device information
  packet in `key_request`, `noise_probe` and `rename_probe`, and BlueZ's `Alias` in
  `rename_probe`. Either can be a personal name.

Cut addresses and personal names out before pasting (rule 10 of
[docs/README.md](../README.md#privacy)).

## `key_request`

Asks a pair for its proximity keys and shows what comes back: a debugger for a key
request that fails, on a new model, say. How the app takes the keys:
[app/keystore.md](../app/keystore.md#which-keys-are-taken).

```bash
cargo run --example key_request -- <MAC> [SECONDS]
```

It sends the key request and lists every packet that arrives for `SECONDS`, 5 by
default: each packet's opcode, length and kind. A key packet (`0x31`) is laid out key by
key - its total length, each key's type and length - and followed by whether the app
would take its ENC_KEY. If no key packet arrives in the window, it says so and lists
what did arrive.

- **Never prints** the key bytes.
- **Stores nothing.** The app asks for the keys on every connection and keeps them
  itself; a debugging run must not replace a key that works.

When it fails:

- **The connect is refused:** the pair is not paired with this machine, not powered on,
  or not in range.
- **No key packet arrives:** run again with a longer window. A Pro 3 sent it 350 ms and
  385 ms after the request
  ([protocol/aap/keys.md](../protocol/aap/keys.md#step-4-wait-for-key-response)), but
  another model, or a pair busy with something else, may take longer (a guess). For a client of your own: nothing sent before the
  handshake is answered reaches the pods
  ([protocol/aap/session.md](../protocol/aap/session.md#step-2-send-handshake)).

## `irk_probe`

Checks that a pair's BLE advertising addresses resolve with the IRK the pair sends over
AAP: the hardware check #134 rested on, kept for the next model. How the app identifies
an advertisement: [app/identification.md](../app/identification.md).

```bash
cargo run --example irk_probe -- <MAC> [<MAC>...]
```

It asks each pair named for its keys and keeps them in memory only. Each pair must be
connected when the probe starts; it can be disconnected once its keys are in. Then it
scans.

- **Each new address** is checked once against every pair's IRK, byte-reversed as the
  app uses it. If that fails, the IRK as sent is tried too; an address that resolves
  only that way means the app needs changing (no model seen so far does that).
- **Each advertisement** is also tried against every pair's ENC_KEY with the app's
  suffix rule, so the two ways of identifying a pair can be compared. A line is printed
  when an address's cleartext bytes 3-8 change, or when the key disagrees with the IRK.
- **A line typed** while it runs is printed as a timestamped marker, to tie a phase -
  lid closed, disconnected - to the output. `q` prints a summary per address and quits.

Pairs are numbered in the order given, addresses in the order heard.

- **Never prints** keys, and no address at its default log level. An error names the
  pair by its number, with its address cut out. The default level leaves out the AAP
  client's warnings: its one warning, a connect still pending after
  `aap::client::CONNECT_LIMIT`, names the address, and the error that follows says the
  same without it. With `RUST_LOG` at `debug`, the library's lines print addresses.
- **Stores nothing.**

If a pair's keys seem to have changed after re-pairing: that is not a given
([protocol/aap/keys.md](../protocol/aap/keys.md#key-persistence)). If they did, the app
picks up the new ones at the pair's next AAP connection, as it asks on every one.

## `decrypt_probe`

Decrypts advertisements captured from Pro 3 and Pro 2 (USB-C), held in the example
itself, with each key in the app's keystore. Needs no pair and no Bluetooth. What the
plaintext holds:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decrypted-format).

```bash
cargo run --example decrypt_probe
```

For each stored pair, it prints every captured payload whose plaintext the app's suffix
rule (`ble::matches_device`) accepts for that pair: the plaintext, bytes 1-3 read as
levels, and what the old magic-byte check would have said of it. A payload from another
pair prints nothing under that pair's key.

- **Never prints** key bytes.
- **Prints** the stored pairs' MACs, and plaintexts that carry the last three bytes of
  the pair's MAC and of the last host's address.

When a decryption does not match:

- **The wrong key:** the ENC_KEY decrypts, the IRK does not.
- **Another pair's payload:** expected when every stored key is tried. The app resolves
  the address with each IRK first, and the suffix confirms the match.
- **Magic bytes:** do not validate with them. The old check (byte 0 upper nibble `0x0`,
  byte 4 `0x2D`) rejects correct decryptions on current models
  ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#decryption-validation)).

To watch live advertisements decrypt in the app itself, run it with
`RUST_LOG=linuxpods=debug`: [app/logging.md](../app/logging.md#ble-advertisements).

## `apple_sniff`

Logs every Apple manufacturer-data advertisement, proximity pairing or not. It runs the
scanner without the window or the battery provider, so it can run beside the app.

```bash
RUST_LOG=linuxpods=trace cargo run --example apple_sniff
```

It logs each proximity pairing advertisement's address and model, and at `trace` the
scanner's lines for everything else Apple sends. It decrypts nothing.

- **Prints** the random addresses the advertisements come from.

## `noise_probe`

Cycles the noise control modes and classifies every packet that comes back. The packet
format in [protocol/aap/noise-control.md](../protocol/aap/noise-control.md) was verified
with it. It talks to the client directly rather than through the coordinator, so the
pods' answers come unfiltered.

```bash
cargo run --example noise_probe -- <MAC>
```

It drains the startup dump, then sets each mode in turn and prints every packet that
arrives in a window after it, each with what it is: a mode report, a speech level, a
`0x09` setting by sub-command, or unknown. Last, it sends Off again with Off allowed
(`0x34`), back to back as the app does
([protocol/aap/noise-control.md](../protocol/aap/noise-control.md#off-has-a-prerequisite-of-its-own)).
Every mode should be audible: listen while it runs.

- **Never prints** keys or serial numbers.
- **Leaves the pair** in Off, with Off allowed. That allowing Off persists on the pods
  is a guess
  ([protocol/aap/noise-control.md](../protocol/aap/noise-control.md#off-has-a-prerequisite-of-its-own)).

## `conversation_awareness`

Logs what Conversation Awareness sends: the setting from the startup dump, and every
speech level and mode report with its time. With `--duck`, it lowers the pair's volume
on the levels as the app does. What the levels are:
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md).

```bash
cargo run --example conversation_awareness -- <MAC> [SECONDS] [--on] [--duck]
```

It listens for `SECONDS`, 60 by default. Speak, stop and speak again, with audio
playing or not, and read the times back against the protocol document.

- **`--on`:** a pair whose dump says the setting is off has it turned on for the run,
  and off again at its end. The setting is persistent, and most likely synced to the
  user's Apple devices. Interrupted with Ctrl-C, the run cannot put it back: turn it
  off in the app, on a phone, or with a second run. Whether it took shows as levels
  while you speak, and in the next connection's dump.
- **`--duck`:** the levels lower the pair's A2DP volume in BlueZ with the app's own
  logic (`volume::Ducking`): [app/volume.md](../app/volume.md). Each volume set is
  logged with its time, with the volume the transport read back after it, and how long
  that took. It waits for that read-back, so it takes the next level later than the
  app would. What was lowered is put back at the end of the run, but not when the run
  is interrupted with Ctrl-C. Quit the app first, or both lower the volume.

- **Never prints** keys or serial numbers.
- **Prints** the A2DP transport's path with `--duck`, which holds the pair's address.

## `rename_probe`

Reads the name the pods give every host and, given a new name, renames them over AAP
and reads it back. The packet and the runs made with it:
[protocol/aap/rename.md](../protocol/aap/rename.md).

```bash
cargo run --example rename_probe -- <MAC> [NEW NAME]
cargo run --example rename_probe -- <MAC> --sweep
```

Whether a rename took is read in three places, each a step further from it:

1. what the pods send in a window after the rename
2. the device information of a second AAP session, opened once the first is closed
3. BlueZ's `Device1.Name`, before the rename and after the second session. It changes
   only once the pair has disconnected and connected again: then run the probe again
   without a name.

It also prints BlueZ's `Alias`, and changes nothing in BlueZ. A name the app would
refuse - empty, with a NUL, over 127 bytes - is refused before connecting.

- **`--sweep`** tries a list of names instead: characters beyond ASCII, and lengths
  from 32 bytes to the length byte's 255. For each, it renames, opens a second session,
  and compares the name the pods report byte for byte with the one sent. The names past
  the 127 bytes the pods keep go out unchecked, as the app never sends them. It renames
  the pods back to their first name at the end, also when a name ends the sweep early.
- **The name is persistent** on the pods: rename them back with a second run. Apple
  devices keep the name they have.

- **Never prints** serial numbers: of the device information packet, only the name.
