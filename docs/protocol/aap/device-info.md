# AAP device information

The device information packet (`0x1D`): its layout, its fields, and how the firmware
version is packed into field 10. The layout and the fields' values were seen on AirPods
Pro 3 and Pro 2 (USB-C) (#160, #161); the fields' names are LibrePods' and a dissector's;
the version's layout is read off one value, seen on both pairs on 2026-11-02, and not yet
confirmed by a second firmware.

Read by `aap::info`; what the app reads and logs of it:
[app/firmware.md](../../app/firmware.md), [app/names.md](../../app/names.md),
[app/logging.md](../../app/logging.md#device-information).

Byte offsets count from 0 at the first byte of the packet.

The pods send the packet unasked at the start of every AAP connection: a run of strings
about themselves, serial numbers among them. It comes in the startup dump, right after
`0x02` (Pro 3, #71 captures 1 and 2). On a new link the dump waits for the second
`0x0C`, and once, on a pair already connected, it came before it: see the table in
[session.md](session.md#initial-state-report).

**Source:** the fields' names from [LibrePods](https://github.com/librepods-org/librepods)
(`docs/device-info.md`) and from the dissector it points to,
[pabloaul/apple-wireshark](https://github.com/pabloaul/apple-wireshark)
(`plugins/aacp/aacp_enums.lua`, at 2e98222), for protocol facts only. Each name below is
theirs until a run here confirms what the field holds; the values are from our own logs.

## Layout

```
Offset:  0  1  2  3  4  5  6  7  8  9  10  11 ...
Hex:     04 00 04 00 1D 00 02 LL LL 04 00  [field 0] 00 [field 1] 00 ... [field 10] 00 [binary bytes]
         ~~~~~~~~~~  ~~    ~~ ~~~~~ ~~~~~  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
         Header      Cmd       Len         NUL-terminated strings, the name first
```

- **Byte 6** was `02` in every one seen, and **bytes 9-10** `04 00` (#160). What the app
  does with a `0x1D` without them: [app/logging.md](../../app/logging.md#device-information).
- **Bytes 7-8** are a little-endian u16 length of what follows: `f8 00`, 248, in a
  257-byte packet. The `04 00` after it counts (#160).
- **Then NUL-terminated strings**, counted from 0 below. After field 10 come binary bytes
  and what looks like a Unix time, not read here. The dissector names two UUIDs, one per
  pod, and the first time each pod was paired (#161).

## Fields

What each held in our logs - a Pro 3 on 2026-11-01, a Pro 2 (USB-C) on 2026-11-02 (#161) -
with the serial numbers left out:

| # | LibrePods' name | dissector's name | Pro 3 | Pro 2 (USB-C) |
|---|---|---|---|---|
| 0 | name | Name | the pods' name | the pods' name |
| 1 | model number | Model Identifier | `A3064` (`A3063` in other sessions) | `A3048` |
| 2 | manufacturer | Manufacturer | `Apple Inc.` | `Apple Inc.` |
| 3 | serial number | Serial Number (System) | (10 characters) | (10 characters) |
| 4 | "Version 1" | Firmware Version (Active) | `90.3431000025000000.6807` | `90.3431000025000000.6807` |
| 5 | "Version 2" | Firmware Version (Pending) | `90.3431000025000000.6807` | `90.3431000025000000.6807` |
| 6 | hardware revision (?) | Hardware Version | `1.0.0` | `1.0.0` |
| 7 | updater app version (?) | EA Protocol Name | `com.apple.accessory.updater.app.71` | same |
| 8, 9 | serial numbers, left and right pod | Serial Number (Left/Right Bud) | | |
| 10 | "Version (?)" | Marketing Version | `9442752` | `9442752` |

- **Fields 4 and 5** hold the same build in Apple's internal form, most likely - a
  guess - and are equal while no update is pending: the dissector's Active and Pending.
  An iPhone shows `9A348` beside them, not this string (2026-11-02, #161).
- **The model number** (field 1) changed between sessions of the same Pro 3, from
  `A3063` to `A3064`, with the rest of `0x1D` the same (#71 capture 2). Most likely each
  pod reports its own, and whichever is primary answers - a guess (#161).
- **The case's version** (the iPhone's "case version") is not in `0x1D` (#161). The
  dissector's `0x23` Case Info carries a `caseVersion`; not read here.

What the app reads and logs of the fields: [app/logging.md](../../app/logging.md#device-information), [app/firmware.md](../../app/firmware.md#each-field-on-its-own).

## The name: field 0

The pods' own name, the one BlueZ shows as `Device1.Name`. See [`rename.md`](rename.md) for the rename packet, the runs, and the name beside BlueZ's alias.

## The firmware version: field 10

The iPhone's Version is field 10, packed. Field 10, the dissector's Marketing Version,
held `9442752` = `0x9015C0` on both pairs on 2026-11-02, when the iPhone showed `9A348`
as the Version of both (#161). Read as nibbles, it is `9A348`:

| bits | | `0x9015C0` |
|---|---|---|
| 20 and up | major version - past 9, a guess | `9` |
| 16-19 | letter, `A` = 0 | `A` |
| 4-15 | build, in decimal | `0x15C` = `348` |
| 0-3 | suffix letter, `a` = 1, none for 0 - a guess | `0` |

That is one value, seen on two pairs, and the layout is ours, read off that value: not
the dissector's - it prints the field as a string - and not confirmed by a second
firmware. Two parts of it are guesses on top:

- **The low nibble** is unknown. Read as a lowercase suffix letter, `a` for 1, as Apple's
  beta builds end in one; nothing for 0, as here. LibrePods' doc quotes `8454371` for the
  field from their pair, `0x8100E3`, which reads `8B14c` so: no release we know of, so
  maybe a beta - whose versions run like `8B5014c`, so the build field may hold more than
  this one value shows.
- **Bits 20 and up** are all the major's, so a major of 10 would read `10`, not wrap.

How the app finds and checks the field: [app/firmware.md](../../app/firmware.md#the-firmware-version-field-10).

## Runs

### The firmware version (2026-11-02)

The iPhone showed **`9A348`** as the Version of both pairs, and `9A348` for the Pro 3's
case. The string `9A348` was nowhere in a fresh `0x1D` from the Pro 3 that day, nor in
the rest of that session's startup dump (38 packets, searched as ASCII and UTF-16). Field
10 held `9442752` on both pairs (#161).

## Open questions

- At the next firmware update on either pair: field 10 against the iPhone's Version - the layout above, and what the low nibble holds, if anything.
- While another host holds the pods: how long `0x1D` waits with the rest of the startup dump, and whether it comes at all - see [`rename.md`](rename.md#open-questions).
- What fields 6 and 7 hold, which LibrePods marks with "(?)", and the binary bytes after field 10.
