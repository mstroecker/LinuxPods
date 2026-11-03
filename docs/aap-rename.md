# AAP Rename

The pods have a name of their own: the one they report to a host as it connects, and the one BlueZ shows as `Device1.Name`. Apple devices keep a name of their own for the pair (see the hardware run). This document describes the packet that changes it, where the pods report it, and what a rename does on this computer and on Apple devices. Read by `aap::name`; sent by `Command::Rename`, from the Name row of the device page's Info group.

**Status:** the rename packet confirmed on AirPods Pro 3 on 2026-11-02 with `examples/rename_probe` on 67e0365 (#160), the name in `0x1D` seen on a Pro 3 and a Pro 2 (USB-C) in earlier runs' logs. That a Pro 2 (USB-C) takes a rename the same way is assumed, not checked, as for the rest of AAP. The app's rename has run against the fake world only.

**Source:** the packet from [LibrePods](https://github.com/librepods-org/librepods) (`linux/airpods_packets.h`, and `AACPManager.createRenamePacket` in its Android app), for protocol facts only: GitHub detects no license for LibrePods, so no code is taken from it. The limit of 127 bytes comes from the name run below.

## Two names

- **The pods' name**: stored on the pods. They send it in the device information packet (`0x1D`) at the start of every AAP connection, and BlueZ reads it as `Device1.Name` when the pair connects.
- **BlueZ's alias**: `Device1.Alias`, this computer's label for the pair, which GNOME Settings' rename sets and which never reaches the pods (#40, PR #62). Until one is set, BlueZ gives the name as the alias, so the alias follows the pods' name. Once set, it stays whatever the pods are called. Setting it to an empty string clears it; typing the old name back in GNOME does not.

The app never touches the alias, neither setting nor clearing it. Set on every rename, it would pin the name, so that a later rename on the pods - from an iPhone, say - never showed here; cleared, it would throw away a name the user gave in GNOME. With no alias set, the new name shows in GNOME at the next reconnect on its own. Whether one is set the app reads as `Alias != Name`, from the names attachment reads with `GetManagedObjects` on every connection BlueZ confirms (`bluez::DeviceNames`). An alias set in GNOME while the pair stays connected is seen at its next connection event.

The Name row shows the pods' name where the link has reported it, else BlueZ's alias, and opens the dialog only once the link has reported the pods' name: started from the alias, Enter would rename the pods to it. The dialog's Rename stays insensitive for the name the pods have.

Elsewhere - the switcher in the header bar, a low battery notification - the app names a pair as the rest of the desktop will once BlueZ has caught up (`Snapshot::display_name`): an alias set in BlueZ, else the pods' name where the link has it, else BlueZ's alias, then the model. Named by the alias alone, the switcher would keep the old name after a rename until the pair connected again: BlueZ's `Name` and `Alias` stayed old until then in the Pro 3 run above (#160). An alias typed back to the pods' old name is the exception: it reads as unset until the pair connects again, so the switcher shows the pods' new name until then, and the alias after; the dialog's body is wrong then too, saying Settings shows the new name once the AirPods connect again, when Settings keeps the alias. Off AAP the pods' name is not known, so a pair renamed and gone shows the old name until BlueZ reads the new one.

## Rename: `0x1A`

```
Offset:  0  1  2  3  4  5  6  7  8  9 ...
Hex:     04 00 04 00 1A 00 01 LL 00 [UTF-8 name, LL bytes]
         ~~~~~~~~~~  ~~       ~~    ~~~~~~~~~~~~~~~~~~~~~~
         Header      Cmd      Len   Name
```

- **Opcode `0x1A`.** LibrePods' own `docs/opcodes.md` lists rename as `0x1E`; both its apps send `0x1A`, and `0x1A` worked. `0x1E` was not tried.
- **The length byte counts bytes**, not characters. LibrePods refuses names over 32 characters, counted in UTF-16 units, so a name with `’` and the like can pass its check and run past 32 bytes.
- **Nothing answers it.** In the 15 s after the rename the pods sent nothing at all. Fire and forget, like the noise mode: the app records the name on the link once the packet is away (`Link::name`).
- **127 bytes at most.** The pods keep up to 127 bytes and cut a longer name silently at a byte - see the name run. The app refuses an empty name, a NUL and anything over 127 bytes before anything is sent: in the dialog, in `Coordinator::command` before the command is queued, and in `aap::name::rename_packet`.

## Device information: `0x1D`

```
Offset:  0  1  2  3  4  5  6  7  8  9  10  11 ...
Hex:     04 00 04 00 1D 00 02 LL LL 04 00  [name] 00 [model number] 00 [serial numbers] ...
         ~~~~~~~~~~  ~~    ~~ ~~~~~ ~~~~~  ~~~~~~~~~
         Header      Cmd       Len         NUL-terminated strings, the name first
```

- Byte 6 was `02` in every one seen. A `0x1D` without it, or without `04 00` at bytes 9-10, is not read for a name (`aap::name::is_info_packet`), as a `0x09` report is not read for a mode unless byte 6 names the setting; it is redacted all the same, by its opcode.
- Bytes 7-8 are a little-endian u16 length of what follows: `f8 00`, 248, in a 257-byte packet. The `04 00` after it counts.
- Then NUL-terminated strings, the name first. The rest holds the model number and the serial numbers.

Only the name is read (`aap::name::parse_info_name`), and only the name is logged or printed: `aap::keys::redacted_hex` cuts a `0x1D` packet to its 11-byte preamble wherever a packet is logged - the read loop's debug and trace lines, the probes - as it cuts a key packet to its preamble. A name that is not UTF-8, as a name the pods cut inside a character would be, is not taken.

The fake pods send `0x1D` laid out so, with made-up stand-ins for the model and serial numbers, and take a rename (`src/harness/pods.rs`).

## Hardware run (Pro 3, 2026-11-02)

`rename_probe` (on 67e0365) opened AAP as the app does, renamed the pair to "Test Pods", listened 15 s, closed the session and opened a second one with the Bluetooth link still up, and read BlueZ's `Name` and `Alias` before and after. The adapter's Device ID was the default (`usb:v1D6Bp0246d0557`, `DeviceID` unset in `main.conf`), not Apple's.

```
BlueZ before: Name "<old name>", Alias "<old name>"
startup dump:   +0.01s device information, name "<old name>" (257 bytes)
renaming to "Test Pods"
                +5.00s sent 04 00 04 00 1a 00 01 09 00 54 65 73 74 20 50 6f 64 73
                (nothing in the next 15 s)
second session: +22.15s device information, name "Test Pods" (242 bytes)
BlueZ after:  Name "<old name>", Alias "<old name>"
```

Then the pair was disconnected and connected again:

```
BlueZ before: Name "Test Pods", Alias "Test Pods"
startup dump:   +0.00s device information, name "Test Pods" (242 bytes)
```

Then the pods connected to the iPhone twice. The iPhone still showed the old name, and AAP here still reported "Test Pods" afterwards.

So:

- **No vendor ID spoofing needed.** The rename took with the adapter's default Device ID. LibrePods doesn't list it among its spoofing features either.
- **The pods take it at once**: the next AAP session reports it, without a reconnect.
- **BlueZ's `Name` catches up at the next reconnect**, not before. With no alias set, the alias and GNOME follow then. The dialog says so, and the app forces no reconnect.
- **Apple devices keep their own name.** The iPhone neither read the new name from the pods nor pushed its own back. The name there seems to live with the Apple ID; re-pairing on the iPhone might pick ours up, but removes the pods from every device on the account, so it was not tried. The dialog says so too.

## Name run (Pro 3, 2026-11-02)

`rename_probe --sweep` (the cap lifted to 255 bytes; the sweep still reaches past the app's 127 through `Client::rename_unchecked`) renamed to each name, opened a second session, and compared the `0x1D` name byte for byte with what was sent:

| name | bytes | result |
|---|---|---|
| umlauts, `Müllers Pöds` | 14 | kept |
| CJK, `耳机测试` | 12 | kept |
| emoji outside the BMP, `Pods 🎧` | 9 | kept |
| ZWJ emoji sequence, `Pods 👩‍💻` | 16 | kept |
| combining accent, `Café Pods` (`e` + U+0301) | 11 | kept |
| Arabic, right to left | 12 | kept |
| ASCII | 32 | kept |
| ASCII | 33 | kept |
| 17 umlauts | 34 | kept |
| ASCII | 64 | kept |
| ASCII | 128 | **cut to its first 127 bytes** |
| ASCII | 248 | **cut to its first 127 bytes** |
| ASCII | 255 | **cut to its first 127 bytes** |

So the pods hold 127 bytes - most likely a 128-byte buffer with its NUL - and cut a longer name silently, at a byte: a name cut inside a multi-byte character would be left as invalid UTF-8. Nothing answered any of the renames. Renamed back at the end.

A 127-byte ASCII name, then, through a reconnect: a second session's `0x1D` had it whole, and after the reconnect BlueZ's `Name` and `Alias` both held all 127 bytes. So the longest name the pods take shows whole on this desktop too.

Any name up to 127 bytes is allowed, whatever its characters; over 127 bytes is refused, never sent, and never cut by bytes. In the dialog the entry's `max-length` of 127 stops a text at 127 characters, and cuts a longer paste to them, exact for ASCII; a shorter name with characters beyond ASCII can still pass 127 bytes, and gets the error style and a line saying it is too long, with Rename insensitive.

## Hardware checks still open

- Rename from the window on the Pro 3: the name in the next session, and after a reconnect in GNOME.
- With an alias set in GNOME Settings: the dialog's other body, and the alias left as it was.
- A name over 127 bytes refused in the dialog; a failed send's toast.
- After a rename, the switcher shows the new name at once, and one reconnect shows it in GNOME, without a second reconnect.
- With an alias set in GNOME Settings, the switcher keeps the alias after a rename.
- A rename while another host - the iPhone - is active, before `0x0C` names this host. And does `0x1D` itself wait for `0x0C`, as the noise mode's report does? Then the Name row stays insensitive, on the alias, for as long as the other host is active.
- The dialog on real GNOME: the focus, the cursor at the end, Enter, a long paste cut to 127 characters.
- Whether a rename on the iPhone reaches the pods - it would come as `0x1A` from the phone - and so shows here at the next reconnect.
