# In-ear

Where LinuxPods takes each pod's in-ear from, and how it reads the ear report across a
role switch. The packets are the pods':
[protocol/aap/ear.md](../protocol/aap/ear.md). What the media pause does with the
places: [media-pause.md](media-pause.md).

## Why AAP and not the advertisement

Why the advertisement's in-ear is no match for the ear report:
[protocol/aap/ear.md](../protocol/aap/ear.md#why-aap-and-not-the-advertisement).

Why so few advertisements reach the app while a pair is connected: where the scanner
discovers, most likely its discovery filter, which sets only `Transport`, leaves BlueZ to
drop duplicates, so what reaches the app is the payload's changes - a guess, not checked.
Through an advertisement monitor there is no discovery filter: see
[Hardware checks](#hardware-checks). What bluetoothd
does around discovery: [protocol/linux/bluez.md](../protocol/linux/bluez.md#discovery).

So in-ear comes from the link while it is up:

| The link | In-ear |
|---|---|
| Has sent both the ear report (`0x06`) and the primary pod packet (`0x08`) | From the link (`Link::ear`, an `aap::ear::EarState`) |
| Up, but has not sent both yet | From the cached advertisement |
| Gone | From the cached advertisement |

**Never take the primary pod from the advertisement** (#71). Each pod calls itself
primary in its own, and a connected pair's payload changes seldom and lags its ears: 4
advertisements reached the app in 70 s (#142).

The ear state goes with the link, like the rest of AAP ([state.md](state.md#what-a-snapshot-shows)).
Each change of a pod's place in it also goes out as an `EarEvent`
([state.md](state.md#events)).

## The packets

The ear report's places: [protocol/aap/ear.md](../protocol/aap/ear.md#ear-report-0x06).

- **The ear report reads only with the primary pod packet:** it gives the primary pod's
  place first.
- **Of the primary pod packet only `PP` is read,** and bytes 7-9 are not checked. A `PP`
  other than `01` or `02` is unknown, which leaves the ear report unreadable.
- **A place other than in an ear or out is unknown, not out:** the app keeps the
  advertisement's in-ear for that pod.
- **The window shows in-ear only,** so `02` and `03` both read as "not in an ear". The
  media pause tells them apart ([media-pause.md](media-pause.md)).

## The role switch

The sequence the pods send at a role switch, and its timings on each model:
[protocol/aap/ear.md](../protocol/aap/ear.md#the-role-switch).

The `0x06` in the new order comes first, before the `0x08` that names the new primary.
Read with the old primary in between, it swaps the pods: the one just taken out shows in
an ear, the one still worn out. The window's render loop wakes at the first change and
draws it, so in a pipeline test a watcher shaped like it saw the swap at 4 of 5 switches,
even with a 1 ms gap.

So `EarState` holds back a report that puts the primary in an ear and the secondary out,
where the last one had the primary out and the secondary in. It holds it:

- until the next `0x08`, which it is then read with;
- until another `0x06`, which replaces it;
- for `aap::ear::ROLE_SWITCH_HOLD` at most, after which it is taken as it is. The hold
  is well above the longest gap seen on either model.

While a report is held, the read loop waits no longer than the hold for the next packet.

In session 3 of the runs ([protocol/aap/ear.md](../protocol/aap/ear.md#session-3-pro-2-usb-c-2026-11-02)),
on a Pro 2 (USB-C), all eight first `0x06`s were held, and each was read with the `0x08`
that followed; none waited out `ROLE_SWITCH_HOLD`.

## Hardware checks

The report held back at a role switch ran on a Pro 2 (USB-C), in session 3 above. The
open questions about the packets are in
[protocol/aap/ear.md](../protocol/aap/ear.md#open-questions).

- How many advertisements reach the app from a connected pair while it listens through
  an advertisement monitor rather than discovery
  ([bluez.md](bluez.md#listening-for-the-pods)): not counted.
