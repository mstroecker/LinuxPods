# AAP Ear Detection

How AirPods tell a connected host which pod is in an ear, over AAP: the ear report (opcode `0x06`) and the primary pod (opcode `0x08`). Read by `aap::ear`, and kept per link by `aap::ear::EarState`.

Captured on AirPods Pro 3 on 2026-11-01, in #142. That AirPods Pro 2 (USB-C) send the same is assumed, not checked: an accepted risk.

## Why AAP and not the advertisement

While a pair is connected, its advertised payload changes seldom and lags its ears: 4 advertisements reached the app in 70 s of the capture, while the ear reports came within 0.5 s of each change. The pods advertise far more often than that (see `ble-proximity-pairing.md`); most likely the scanner's discovery filter, which sets only `Transport`, leaves BlueZ to drop duplicates, so what reaches the app is the payload's changes. Nor do the advertisements agree with each other: each pod advertises from its own side with its own status byte, and with both pods worn two of them read `in_ear=false/true` and `true/false` within 0.6 s.

So while the link is up, in-ear comes from these two packets. The advertisement's in-ear shows only before the link has sent both, and after it goes.

## The packets

Both come unasked, on every change, and once each right after the link comes up, `0x08` first.

### Primary pod: `0x08`

```
04 00 04 00 08 00 PP 00 SS TT
                  ^^ primary pod: 01 left, 02 right
```

10 bytes long.

| Byte | Seen | Reading |
|---|---|---|
| 6 (`PP`) | `01`, `02` | The primary pod: `01` left, `02` right. |
| 7 | `00` | Always `00`. |
| 8 (`SS`) | `01`, `00` | `01` while the secondary pod can be reached, `00` with it in the closed case (once), matching `0x06`'s `03`. |
| 9 (`TT`) | `01`, `00` | `01` in the first packet of a role switch, `00` in a second one 30-90 ms later. Possibly "switch in progress". `PP` is the new primary in both. |

`PP` agreed every time with the advertisement's primary flag (status bit 5), 13 times across the logs, and with the battery packet, which lists the primary pod first (`03 04 …` with `08 01`, `03 02 …` with `08 02`).

Only `PP` is read. Any value goes in bytes 7-9, and a `PP` other than `01` or `02` is unknown, which leaves the ear report unreadable.

### Ear report: `0x06`

```
04 00 04 00 06 00 PE SE
                  ^^ primary pod's place
                     ^^ secondary pod's place
```

8 bytes long. The report names no pod: it gives the primary pod's place first, so it reads only with the primary pod beside it.

| Place | Meaning |
|---|---|
| `00` | In an ear |
| `01` | Out of the ear, out of the case |
| `02` | In the case, lid open |
| `03` | In the closed case |

- `03` came about 4 s after the lid closed on the secondary pod; at the close itself `00 02` was sent again, unchanged. With `03` the battery packet reports that pod as Disconnected (status `04`, level `00`; #108).
- A pod in the case never became primary. With both in the closed case, the pods drop the link.
- Any other byte is unknown, not out: the app keeps the advertisement's in-ear for that pod.

The UI shows in-ear only, so `02` and `03` both read as "not in an ear".

A `0x0D` report of the unchanged noise control mode usually follows an ear report - see `aap-noise-control.md`.

## The role switch

Taking the secondary pod out changes nothing but `0x06`.

When the primary pod comes out, the pod still in an ear becomes primary 0.85-1.2 s later. A new `0x08` and a `0x06` in the new order then arrive within a few milliseconds of each other, and at all six switches captured the `0x06` came first, 1-31 ms before the `0x08`.

Read with the old primary in between, that `0x06` swaps the pods: the one just taken out shows in an ear, the one still worn out. The window's render loop wakes at the first change and draws it, so in a pipeline test a watcher shaped like it saw the swap at 4 of 5 switches even with a 1 ms gap. `EarState` therefore holds back a report that puts the primary in an ear and the secondary out, where the last one had the primary out and the secondary in: until the next `0x08`, which it is then read with; until another `0x06`, which replaces it; or for `ROLE_SWITCH_HOLD` (250 ms), after which it is taken as it is.

## The capture

Session 1 (`RUST_LOG=linuxpods=debug`, first 8 bytes of each packet), selected rows:

| UTC | `0x08` before | `0x06` | Then, in arrival order | Reading |
|---|---|---|---|---|
| 16:58:43.6 | `02` (right) | `00 01` | | left out (secondary) |
| 16:58:58.4 | | `00 00` | | left back |
| 16:59:00.6 | `02` (right) | `01 00` | `06 00 01`, `08 01` at 16:59:01.5 | right out (primary); left becomes primary |
| 16:59:07.2 | | `00 00` | | right back |
| 17:04:15.7 | `01` (left) | `00 01` | | right out (secondary) |
| 17:04:19.7 | `01` (left) | `01 00` | `06 00 01`, `08 02` at 17:04:20.6 | left out (primary); right becomes primary |
| 17:16:22.0 | `02` (right) | `01 00` | `06 00 01`, `08 01`, battery `03 04` at 17:16:23.2 | right out (primary); left becomes primary |

Session 2 (a local build logging whole packets), pods in the case, bytes from the opcode on:

| UTC | Packet | Pods |
|---|---|---|
| 17:21:35.0 | `08 00 01 00 01 00` | connect, left primary |
| 17:22:25.1 | `08 00 02 00 01 01`, then `… 01 00` 87 ms later | switch to right |
| 17:22:47.4 | `06 00 00 02` | secondary (left) in the case, lid open |
| 17:22:58.9 | `06 00 00 02` | lid closed: sent again, unchanged |
| 17:23:02.9 | `06 00 00 03`, `08 00 02 00 00 00` | 4 s later: the secondary in the closed case |
| 17:23:17.7 | `06 00 00 02`, `08 00 02 00 01 00` | lid open |
| 17:23:18.1 | `06 00 00 01` | out of the case |
| 17:23:41.8 | `08 00 01 00 01 01`, then `… 01 00` | switch to left |
| 17:24:18.3 | `08 00 02 00 01 01`, then `… 01 00` | switch to right |
| 17:24:20.8 | `06 00 01 02` | primary (right) out, secondary in the case: no switch |
| 17:24:21.3 | | link reset by the pods once both were in the closed case |
| 17:24:40.2 | `08 00 02 00 01 00`, `06 00 00 02` | reconnect: right in an ear, left still in the open case |

Switch timing, `0x06` before the first `0x08`: 17:22:25.062 vs .065, 17:23:41.809 vs .834, 17:24:18.309 vs .311.

The `AAP packet (N bytes):` debug line shows the first 8 bytes of a packet; at `trace`, the whole packet follows (key packets redacted).

## What the fake does

The fake AirPods (`harness/pods.rs`) send `0x08` then `0x06` right after the handshake's answer, a `0x06` on every change of a pod's place, and on a switch the `0x06` in the new order, then two `0x08`s - the first 10 ms later with `TT` set, the second 60 ms after it. A pod in the case is in the closed case the moment the lid closes, where the hardware took about 4 s.
