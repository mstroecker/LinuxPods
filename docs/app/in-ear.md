# In-ear

Where LinuxPods takes each pod's in-ear from, and how it reads the ear report across a role switch.

## Why AAP and not the advertisement

Why the advertisement's in-ear is no match for the ear report: [protocol/aap/ear.md](../protocol/aap/ear.md#why-aap-and-not-the-advertisement).

Why so few advertisements reach the app while a pair is connected: most likely the
scanner's discovery filter, which sets only `Transport`, leaves BlueZ to drop
duplicates, so what reaches the app is the payload's changes. What bluetoothd does
around discovery: [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery).

So while the link is up, in-ear comes from the ear report (`0x06`) and the primary pod packet (`0x08`). The advertisement's in-ear shows only before the link has sent both, and after it goes.

## The packets

The ear report's places: [protocol/aap/ear.md](../protocol/aap/ear.md#ear-report-0x06).

Of the primary pod packet only `PP` is read, and any value goes in bytes 7-9. A `PP` other than `01` or `02` is unknown, which leaves the ear report unreadable.

Any other byte is unknown, not out: the app keeps the advertisement's in-ear for that pod.

The UI shows in-ear only, so `02` and `03` both read as "not in an ear".

## The role switch

The sequence the pods send at a role switch: [protocol/aap/ear.md](../protocol/aap/ear.md#the-role-switch).

Read with the old primary in between, the `0x06` in the new order swaps the pods: the one just taken out shows in an ear, the one still worn out. The window's render loop wakes at the first change and draws it, so in a pipeline test a watcher shaped like it saw the swap at 4 of 5 switches even with a 1 ms gap. `EarState` therefore holds back a report that puts the primary in an ear and the secondary out, where the last one had the primary out and the secondary in: until the next `0x08`, which it is then read with; until another `0x06`, which replaces it; or for `ROLE_SWITCH_HOLD` (250 ms), after which it is taken as it is.

In session 3 of the runs ([protocol/aap/ear.md](../protocol/aap/ear.md#session-3-pro-2-usb-c-2026-11-02)), all eight first `0x06`s were held, and each was read with the `0x08` that followed; none waited out `ROLE_SWITCH_HOLD`.
