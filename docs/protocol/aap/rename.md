# AAP rename

The rename packet (`0x1A`), the pods' own name, and what a rename does on this computer and on Apple devices.

The pods have a name of their own: the one they report to a host as it connects, and the one BlueZ shows as `Device1.Name`. Apple devices keep a name of their own for the pair (see the hardware run). This document describes the packet that changes it, where the pods report it, and what a rename does on this computer and on Apple devices. The packet is built by `aap::name`, and the name read by `aap::info`; sent by `Command::Rename`, from the Name row of the device page's Info group.

**Status:** the rename packet confirmed on AirPods Pro 3 on 2026-11-02 with `examples/rename_probe` on 67e0365 (#160), the name in `0x1D` seen on a Pro 3 and a Pro 2 (USB-C) in earlier runs' logs. That a Pro 2 (USB-C) takes a rename the same way is assumed, not checked, as for the rest of AAP.

**Source:** the packet from [LibrePods](https://github.com/librepods-org/librepods) (`linux/airpods_packets.h`, and `AACPManager.createRenamePacket` in its Android app), for protocol facts only: GitHub detects no license for LibrePods, so no code is taken from it. The limit of 127 bytes comes from the name run below.

## Two names

- **The pods' name**: stored on the pods. They send it in the device information packet (`0x1D`) at the start of every AAP connection, and BlueZ reads it as `Device1.Name` when the pair connects.

BlueZ's alias, the other name: [protocol/linux/bluez.md](../linux/bluez.md#device1name-and-device1alias). How the app uses the two: [app/names.md](../../app/names.md#two-names).

## Rename: `0x1A`

```
Offset:  0  1  2  3  4  5  6  7  8  9 ...
Hex:     04 00 04 00 1A 00 01 LL 00 [UTF-8 name, LL bytes]
         ~~~~~~~~~~  ~~       ~~    ~~~~~~~~~~~~~~~~~~~~~~
         Header      Cmd      Len   Name
```

- **Opcode `0x1A`.** LibrePods' own `docs/opcodes.md` lists rename as `0x1E`; both its apps send `0x1A`, and `0x1A` worked. `0x1E` was not tried.
- **The length byte counts bytes**, not characters. LibrePods refuses names over 32 characters, counted in UTF-16 units, so a name with `’` and the like can pass its check and run past 32 bytes.
- **Nothing answers it.** In the 15 s after the rename the pods sent nothing at all.
- **127 bytes at most.** The pods keep up to 127 bytes and cut a longer name silently at a byte - see the name run.

What the app records, and what it refuses: [app/names.md](../../app/names.md#rename-0x1a).

## Device information: `0x1D`

Where the pods report their name, and the packet's other fields: [`device-info.md`](device-info.md).

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
- **BlueZ's `Name` catches up at the next reconnect**, not before. With no alias set, the alias and GNOME follow then.
- **Apple devices keep their own name.** The iPhone neither read the new name from the pods nor pushed its own back. The name there seems to live with the Apple ID; re-pairing on the iPhone might pick ours up, but removes the pods from every device on the account, so it was not tried.

What the rename dialog says of both: [app/names.md](../../app/names.md#hardware-run).

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

What the app allows: [app/names.md](../../app/names.md#name-run).

## Open questions

- A rename while another host - the iPhone - is active, before `0x0C` names this host. And does `0x1D` itself wait for `0x0C`, as the noise mode's report does?
- Whether a rename on the iPhone reaches the pods - it would come as `0x1A` from the phone - and so shows here at the next reconnect.
