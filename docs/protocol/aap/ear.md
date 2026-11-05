# AAP ear detection

How AirPods tell a connected host where each pod is, over AAP: the ear report (`0x06`)
and the primary pod packet (`0x08`) it reads with, and the role switch between the pods.
Captured on AirPods Pro 3 on 2026-11-01 (#142, #71 capture 4). On AirPods Pro 2 (USB-C)
only the role switch was captured (2026-11-02, #71): the same packets in the same order,
with longer gaps. That the Pro 2 send the rest the same is assumed, not checked.

Read by `aap::ear`; how the app reads it: [app/in-ear.md](../../app/in-ear.md).
What the app does with it beyond the window's icons - pausing the media - is in
[app/media-pause.md](../../app/media-pause.md).

Byte offsets count from 0 at the first byte of the packet.

## Why AAP and not the advertisement

While a pair is connected, its advertisement lags its ears, and its two pods disagree:

- **It lags.** 4 advertisements reached the app in 70 s of the #142 capture (Pro 3),
  while the ear reports came within 0.5 s of each change. The pods advertise far more
  often than that ([proximity-pairing.md](../ble/proximity-pairing.md#update-frequency)).
  Why so few reach the app: [app/in-ear.md](../../app/in-ear.md#why-aap-and-not-the-advertisement).
- **The pods disagree.** Each pod advertises from its own side, with its own status
  byte. With both pods worn, two advertisements read `in_ear=false/true` and
  `true/false` within 0.6 s (#142).

Where the app takes in-ear from: [app/in-ear.md](../../app/in-ear.md#why-aap-and-not-the-advertisement).

## The packets

Both come unasked, on every change, and once each right after the link comes up,
`0x08` first ([session.md](session.md#initial-state-report)).

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
| 9 (`TT`) | `01`, `00` | `01` in the first packet of a role switch, `00` in a second one, `PP` the new primary in both. Possibly "switch in progress", a guess. Once `01` came with no switch, when the second pod came out with both then out, followed by an `00` with `PP` unchanged (Pro 3, #71 capture 4). |

The second `0x08` of a switch came 28-291 ms after the first on the Pro 2 (USB-C), and
31-88 ms after it on the Pro 3 in #142 session 2, the only #142 session that shows `TT`.

`PP` agreed every time with the advertisement's primary flag (status bit 5), 13 times
across the #142 logs. It agreed too with the battery packet, which lists the primary pod
first: `03 04 …` with `08 01`, `03 02 …` with `08 02` ([battery.md](battery.md#order-of-the-entries)).

### Ear report: `0x06`

```
04 00 04 00 06 00 PE SE
                  ^^ primary pod's place
                     ^^ secondary pod's place
```

8 bytes long. The report names no pod: it gives the primary pod's place first, so it
reads only with the primary pod beside it.

| Place | Meaning |
|---|---|
| `00` | In an ear |
| `01` | Out of the ear, out of the case |
| `02` | In the case, lid open |
| `03` | In the closed case |

No other place byte was seen (#142, #71 capture 4).

- **`03`** came about 4 s after the lid closed on the secondary pod: 4 s in #142 session
  2, 4.5 s in #71 capture 4. At the close itself `00 02` was sent again, unchanged. With
  `03` the battery packet reports that pod and the case as Disconnected (status `04`,
  level `00`; [battery.md](battery.md#disconnected-is-no-0), #108).
- **At a lid change** the `0x08` with `SS` changed comes first, then the `0x06`, then the
  battery. #71 capture 4 has them to the millisecond: at the closed case the `0x08` at
  19:42:30.160 and the `0x06` with `03` at .192; at the lid opening the `0x08` at
  19:43:16.296 and the `0x06` with `02` at .383. The `0x0C` that also comes at a lid
  change: [session.md](session.md#0x0c-a-device-in-use). Only at a role switch does
  the `0x06` come first - see below.
- **A pod in the case never became primary.** With both in the closed case, the pods
  drop the link (#142 session 2, #71 capture 4).

What the app shows of each place: [app/in-ear.md](../../app/in-ear.md#the-packets).

A `0x0D` report of the unchanged noise control mode usually follows an ear report - see [noise-control.md](noise-control.md#ear-detection-reports).

## The role switch

Taking the secondary pod out changes nothing but `0x06`.

When the primary pod comes out, the pod still in an ear becomes primary 0.7-1.2 s later,
on both models. A new `0x08` and a `0x06` in the new order then arrive close together,
and at every switch captured the `0x06` came first:

| Model | Switches | `0x08` after the `0x06` | Source |
|---|---|---|---|
| Pro 3 | 6 | 0.2-31 ms | #142, 2026-11-01, as 67e0365 re-read it |
| Pro 3 | 7 | 0-54 ms | #71 capture 4, 2026-11-01 |
| Pro 2 (USB-C) | 8 | 0.5-87 ms | #71, 2026-11-02 |

Both models send the same sequence:

1. the `0x06` in the new order;
2. the `0x08` with `TT` = `01`;
3. the battery, in the new order;
4. the `0x08` with `TT` = `00`;
5. 0.1-0.8 s after the first `0x06`, the same `0x06` and battery again.

Once, at 16:59:01 on the Pro 3, the `0x06` came a second time between the first `0x08`
and the battery.

What that `0x06` does read with the old primary, and how the app holds it: [app/in-ear.md](../../app/in-ear.md#the-role-switch).

## Runs

The app's `debug` and `trace` lines that these come from: [app/logging.md](../../app/logging.md#aap-packets).

### Session 1 (Pro 3, 2026-11-01)

At df17831, `RUST_LOG=linuxpods=debug`, first 8 bytes of each packet (#142). Selected
rows:

| UTC | `0x08` before | `0x06` | Then, in arrival order | Reading |
|---|---|---|---|---|
| 16:58:43.6 | `02` (right) | `00 01` | | left out (secondary) |
| 16:58:58.4 | | `00 00` | | left back |
| 16:59:00.6 | `02` (right) | `01 00` | `06 00 01`, `08 01` at 16:59:01.5 | right out (primary); left becomes primary |
| 16:59:07.2 | | `00 00` | | right back |
| 17:04:15.7 | `01` (left) | `00 01` | | right out (secondary) |
| 17:04:19.7 | `01` (left) | `01 00` | `06 00 01`, `08 02` at 17:04:20.6 | left out (primary); right becomes primary |
| 17:16:22.0 | `02` (right) | `01 00` | `06 00 01`, `08 01`, battery `03 04` at 17:16:23.2 | right out (primary); left becomes primary |

### Session 2 (Pro 3, 2026-11-01)

A local build of df17831 logging up to 64 bytes of each packet (#142), pods in the case,
bytes from the opcode on:

| UTC | Packet | Pods |
|---|---|---|
| 17:21:35.0 | `08 00 01 00 01 00` | connect, left primary |
| 17:22:25.1 | `08 00 02 00 01 01`, then `… 01 00` 87 ms later | switch to right |
| 17:22:47.4 | `06 00 00 02` | secondary (left) in the case, lid open |
| 17:22:58.9 | `06 00 00 02` | lid closed: sent again, unchanged |
| 17:23:02.9 | `08 00 02 00 00 00`, `06 00 00 03` | 4 s later: the secondary in the closed case |
| 17:23:17.7 | `08 00 02 00 01 00`, `06 00 00 02` | lid open |
| 17:23:18.1 | `06 00 00 01` | out of the case |
| 17:23:41.8 | `08 00 01 00 01 01`, then `… 01 00` | switch to left |
| 17:24:18.3 | `08 00 02 00 01 01`, then `… 01 00` | switch to right |
| 17:24:20.8 | `06 00 01 02` | primary (right) out, secondary in the case: no switch |
| 17:24:21.3 | | link reset by the pods once both were in the closed case |
| 17:24:40.2 | `08 00 02 00 01 00`, `06 00 00 02` | reconnect: right in an ear, left still in the open case |

Switch timing, `0x06` before the first `0x08`: 17:22:25.062 vs .065, 17:23:41.809 vs .834, 17:24:18.309 vs .311.

The lid rows are timed to 0.1 s only, which does not settle the order of their two
packets. They are listed in the order #71 capture 4 has them, to the millisecond: the
`0x08` first at a lid change (see the ear report above).

### #71 capture 4 (Pro 3, 2026-11-01)

Build df29b14, `RUST_LOG=linuxpods=trace`: 7 role switches, primary out and back in,
both ways; one pod into the case, the lid closed and opened; both pods into the closed
case. The figures are in the sections above.

### Session 3 (Pro 2 (USB-C), 2026-11-02)

A plain build of d289626 at `RUST_LOG=linuxpods=trace` (#71): both pods worn, the
primary taken out and put back eight times, alternately right and left, 19:27-19:28 UTC.
Each removal switched the primary. What the app made of each switch:
[app/in-ear.md](../../app/in-ear.md#the-role-switch).

| Primary out (`06 00 01 00`) | `0x06` in the new order | First `0x08` (`TT` `01`) | Second `0x08` (`TT` `00`) | New primary |
|---|---|---|---|---|
| 19:27:33.004 | +1200 ms | +0.8 ms | +28 ms | left |
| 19:27:43.808 | +697 ms | +3.3 ms | +116 ms | right |
| 19:27:53.200 | +905 ms | +80.0 ms | +83 ms | left |
| 19:28:02.150 | +944 ms | +5.6 ms | +183 ms | right |
| 19:28:11.812 | +960 ms | +87.1 ms | +291 ms | left |
| 19:28:20.677 | +870 ms | +0.5 ms | +80 ms | right |
| 19:28:27.977 | +795 ms | +55.0 ms | +39 ms | left |
| 19:28:36.936 | +851 ms | +2.0 ms | +96 ms | right |

Each column is measured from the one before it.

## Open questions

- With the app not running: does taking a pod out pause anything? Does any AVRCP pause come from the pods? (`btmon`, or `evtest` on the `(AVRCP)` input device.)
- The pods' own Automatic Ear Detection turned off on the iPhone: are ear reports (`0x06`) still sent?
- What `TT` means, given the `01` that came with no switch.
- Pro 2 (USB-C): the lid, the case and the places other than `00` and `01`, assumed to be as on the Pro 3.
