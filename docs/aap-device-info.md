# AAP Device Information

The pods send the device information packet (`0x1D`) at the start of every AAP connection, unasked: a run of strings about themselves, serial numbers among them. The app reads two of them, the pods' name (#160) and the firmware version (#161), and nothing else. Read by `aap::info`; the name shows in the Name row of the device page's Info group, the version in the Firmware row below it.

**Status:** the packet's layout and the name's place seen on a Pro 3 and a Pro 2 (USB-C), and a rename's name came back in it on a Pro 3 (#160, see [`aap-rename.md`](aap-rename.md)). Field 10 held `9442752` on both pairs on 2026-11-02, when the iPhone showed `9A348` as the Version of both. The way the app reads that value is ours, read off that one value: to be confirmed at the next firmware update on either pair. The app's Firmware row has run against the fake world only.

**Source:** the fields' names from [LibrePods](https://github.com/librepods-org/librepods) (`docs/device-info.md`) and from the dissector it points to, [pabloaul/apple-wireshark](https://github.com/pabloaul/apple-wireshark) (`plugins/aacp/aacp_enums.lua`, at 2e98222), for protocol facts only. Each name below is theirs until a run here confirms what the field holds; the values are from our own logs.

## Layout

```
Offset:  0  1  2  3  4  5  6  7  8  9  10  11 ...
Hex:     04 00 04 00 1D 00 02 LL LL 04 00  [field 0] 00 [field 1] 00 ... [field 10] 00 [binary bytes]
         ~~~~~~~~~~  ~~    ~~ ~~~~~ ~~~~~  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
         Header      Cmd       Len         NUL-terminated strings, the name first
```

- Byte 6 was `02` in every one seen. A `0x1D` without it, or without `04 00` at bytes 9-10, is not read at all (`aap::info::is_info_packet`), as a `0x09` report is not read for a mode unless byte 6 names the setting; it is redacted all the same, by its opcode.
- Bytes 7-8 are a little-endian u16 length of what follows: `f8 00`, 248, in a 257-byte packet. The `04 00` after it counts.
- Then NUL-terminated strings, counted from 0 below. After field 10 come binary bytes and what looks like a Unix time, not read here; the dissector names two UUIDs, one per pod, and the first time each pod was paired.

## Fields

What each held in our logs - a Pro 3 on 2026-11-01, a Pro 2 (USB-C) on 2026-11-02 - with the serial numbers left out:

| # | LibrePods' name | dissector's name | Pro 3 | Pro 2 (USB-C) | read here |
|---|---|---|---|---|---|
| 0 | name | Name | the pods' name | the pods' name | the Name row (#160) |
| 1 | model number | Model Identifier | `A3064` (`A3063` in other sessions) | `A3048` | |
| 2 | manufacturer | Manufacturer | `Apple Inc.` | `Apple Inc.` | |
| 3 | serial number | Serial Number (System) | (10 characters) | (10 characters) | never |
| 4 | "Version 1" | Firmware Version (Active) | `90.3431000025000000.6807` | `90.3431000025000000.6807` | |
| 5 | "Version 2" | Firmware Version (Pending) | `90.3431000025000000.6807` | `90.3431000025000000.6807` | |
| 6 | hardware revision (?) | Hardware Version | `1.0.0` | `1.0.0` | |
| 7 | updater app version (?) | EA Protocol Name | `com.apple.accessory.updater.app.71` | same | |
| 8, 9 | serial numbers, left and right pod | Serial Number (Left/Right Bud) | | | never |
| 10 | "Version (?)" | Marketing Version | `9442752` | `9442752` | the Firmware row (#161) |

- **Fields 4 and 5** hold the same build in Apple's internal form, most likely, and are equal while no update is pending - the dissector's Active and Pending. Not shown: an iPhone shows `9A348` beside them.
- **The model number** (field 1) changed between sessions of the same Pro 3: most likely each pod reports its own, and whichever is primary answers. Worth knowing before an Info row for it.
- **The case's version** (the iPhone's "case version") is not in `0x1D`. The dissector's `0x23` Case Info carries a `caseVersion`; not read here.

Only fields 0 and 10 leave the parser. The serial numbers are never parsed out, logged, kept or shown: `aap::keys::redacted_hex` cuts any `0x1D` to its 11-byte preamble wherever a packet is logged - the read loop's debug and trace lines, the probes - as it cuts a key packet to its preamble. A field that does not read carries none of its bytes in its error either, as a field out of place could be a serial number.

Each field is read on its own (`aap::info::DeviceInfo`): a name that is not UTF-8, as a name the pods cut inside a character would be, is not taken, and the version is all the same; a version that does not read leaves the name. The packet as a whole never fails to decode once its preamble is right.

## The name: field 0

The pods' own name, the one BlueZ shows as `Device1.Name`. See [`aap-rename.md`](aap-rename.md) for the rename packet, the runs, and the name beside BlueZ's alias.

## The firmware version: field 10

The iPhone showed **`9A348`** as the Version of both pairs on 2026-11-02, and `9A348` for the Pro 3's case. The string `9A348` was nowhere in a fresh `0x1D` from the Pro 3 that day, nor in the rest of that session's startup dump (38 packets, searched as ASCII and UTF-16). Field 10, the dissector's Marketing Version, held `9442752` = `0x9015C0`, and read as nibbles it is `9A348`:

| bits | | `0x9015C0` |
|---|---|---|
| 20 and up | major version - past 9, a guess | `9` |
| 16-19 | letter, `A` = 0 | `A` |
| 4-15 | build, in decimal | `0x15C` = `348` |
| 0-3 | suffix letter, `a` = 1, none for 0 - a guess | `0` |

That is one value, seen on two pairs, and the layout is ours, read off that value: not the dissector's - it prints the field as a string - and not confirmed by a second firmware. On top of it, two guesses of the app's own (`aap::info::FirmwareVersion`):

- **The low nibble** is unknown. Read as a lowercase suffix letter, `a` for 1, as Apple's beta builds end in one; nothing for 0, as here. LibrePods' doc quotes `8454371` for the field from their pair, `0x8100E3`, which reads `8B14c` so: no release we know of, so maybe a beta - whose versions run like `8B5014c`, so the build field may hold more than this one value shows.
- **Bits 20 and up** are all the major's, so a major of 10 would read `10`, not wrap.

The field must be decimal digits alone, within a u32, or the version is not read. It is found by its place alone, as the eleventh string, which held on the two models seen; on a model whose `0x1D` has another count of strings, whatever stands eleventh would be read, if it were all digits - unlikely for a serial number, as Apple's mix letters and digits. The app logs field 10's value beside its reading of it as the version arrives - `Firmware reported by <MAC>: 9A348 (Marketing Version 9442752)` - so a wrong reading shows.

**To confirm: the next firmware update** on either pair - its field 10 against the iPhone's Version.

## What the app does with it

- **Kept for the run, connected or not.** The version comes with each AAP connection's `0x1D` and is kept per pair after the link goes (`Inner::firmware_versions`, in the snapshot as `Snapshot::firmware_versions`), like the BLE cache but never expiring, unlike the rest of AAP, which goes with the link; the next connection's replaces it. An update installed through an iPhone shows at the pair's next AAP connection. A `0x1D` read on a link that has since been replaced is dropped, as every packet of the old link is.
- **The Firmware row** in the Info group, below Name, shows the version as the iPhone does, dimmed and selectable, so it can be copied into a bug report. It is no command and opens nothing. Before the pair's first AAP connection in a run there is no version: the row shows "Unknown" and is insensitive, so the group keeps its shape.

The fake pods send `0x1D` laid out so, fields 1-9 made up and field 10 `9442752` until a test changes it (`Pods::set_marketing_version`, `src/harness/pods.rs`), so the fake world shows the row.

## Hardware checks still open

- On the Pro 3: the Firmware row shows `9A348`, matching the iPhone's Version; it stays after the pair disconnects; it shows "Unknown", insensitive, before the pair's first connection in a run.
- At the next firmware update on either pair: field 10 against the iPhone's Version - the layout above, and what the low nibble holds, if anything.
- Whether `0x1D` waits for `0x0C` to name this host, as the noise mode's report does - see [`aap-rename.md`](aap-rename.md).
