# AAP session

How an AAP session comes up - the L2CAP channel, the handshake, the requests that follow
it, and what the pods send unasked - and every opcode in the runs cited here. The packets a client
sends are LibrePods'; the order and the timings are from AirPods Pro 3 captures (#71,
#109) and a Pro 2 (USB-C) run (#149).

Opened by `aap::Client::open`; every packet is read by `aap::decode`. How the app opens a
link: [app/aap-links.md](../../app/aap-links.md).

Byte offsets count from 0 at the first byte of the packet.

## Packets

Every packet after the handshake starts with the header `04 00 04 00`, in both
directions. Byte 4 is the opcode, which names what the packet carries. The handshake
(`00 00 04 00 …`) and its answer (`01 00 04 00 …`) are the only packets that start
otherwise.

- Byte 4 is an opcode only after that header: a packet that starts otherwise can hold
  any value there.
- [LibrePods' control command table](https://github.com/librepods-org/librepods/blob/main/docs/control_commands.md)
  reads bytes 4-5 as one 16-bit little-endian opcode. Byte 5 is `00` in every packet
  these documents record byte for byte.

The [opcode map](#opcodes) below lists each opcode and the document that describes it.

## Step 1: Connect to AirPods

AAP runs on an L2CAP channel to the pair (LibrePods; every run here):

| | |
|---|---|
| Transport | L2CAP over BR/EDR (Classic Bluetooth) |
| PSM | `0x1001` (4097) |
| Socket type | `SOCK_SEQPACKET` |

What the kernel's connect does before the channel is up: [protocol/linux/l2cap.md](../linux/l2cap.md).

## Step 2: Send Handshake

The handshake, 16 bytes (LibrePods):

```
00 00 04 00 01 00 02 00 00 00 00 00 00 00 00 00
```

The answer starts `01 00 04 00`. A Pro 3 sent 18 bytes, the same in every #71 capture:
`01 00 04 00 00 00 01 00 03 00 00 00 00 00 00 00 00 00`.

⚠️ **Wait for the answer before sending anything else.** The pods drop what they are sent
before they have answered, and do not answer it later.

- A Pro 3 that had only just connected answered 1.2 s after the handshake. Requests sent
  after a fixed 500 ms pause were never answered: in two minutes, no battery packet and
  no key packet (406c721, PR #109, 2026-10-20).
- A pair connected before the session opened answered well inside 500 ms (Pro 3, #109).
- The answer took 1.23 s and 1.04 s in #71 captures 1 and 2 (Pro 3).

How the app waits for the answer: [app/aap-links.md](../../app/aap-links.md#step-2-send-handshake).

## Step 3: Send the requests

Once the answer is in, a client sends these (LibrePods). In the #71 captures the app sent
all three within a millisecond:

| Packet | Opcode | What it asks for |
|---|---|---|
| `04 00 04 00 0F 00 FF FF FF FF` | `0x0F` | The notification request: the battery and the reports after it ([Initial State Report](#initial-state-report)). LibrePods says it is needed for notifications such as ear detection and the noise control mode. Its AAP Definitions give `FF FF FE FF` as the last bytes, and say `FF FF FF FF` works too. |
| `04 00 04 00 4D 00 FF 00 00 00 00 00 00 00` | `0x4D` | The features: [below](#prerequisites-the-features-packet). |
| `04 00 04 00 30 00 05 00` | `0x30` | The keys: [keys.md](keys.md#step-3-send-key-request). |

## Prerequisites: the features packet

```
04 00 04 00 4D 00 FF 00 00 00 00 00 00 00
```

- **Nothing answers it** (Pro 3, #71 capture 1). The `0x2B` that comes near it comes
  with the handshake's answer: see [`0x2B`](#0x2b-the-hosts-the-pair-knows).
- **LibrePods** calls it "Set specific features", and says it enables Conversation
  Awareness while audio plays: without it, Conversation Awareness works only while
  nothing plays ([conversation-awareness.md](conversation-awareness.md)).
- **Adaptive needs it**, LibrePods' AAP Definitions say: it "also enables the Adaptive
  Transparency feature" ("Setting specific features"), and without it a pair that has
  Adaptive answers a change to it "with the same packet but with a different mode (like
  0x02)" ("Changing Noise Control").

When the app sends it: [app/aap-links.md](../../app/aap-links.md#the-features-packet).

## Initial State Report

What the pods send on a new link, in order (Pro 3: #71 captures 1 and 2, and all five
connects of capture 4):

1. **With the handshake's answer:** `0x2B`, the hosts the pair knows - in the same
   millisecond as the answer in capture 1, 30 ms after it in capture 2.
2. **After the notification request:** the battery (146 ms after it in capture 1, 188 ms
   in capture 2), the primary pod (`0x08`), a first `0x0C` naming a pod, `0x0E` naming
   this host, then `0x2E` twice and the ear report (`0x06`) in captures 1 and 2, or
   `0x2E`, the ear report and `0x2E` again in capture 4.
3. **After the key request:** the key packet, 350 ms after the request in capture 1 and
   385 ms in capture 2.
4. **The startup dump,** once a second `0x0C` names this host, 1.4-1.5 s after the first.
   It came 57 ms after that `0x0C` in capture 1, and 0.7 ms after it in capture 2.

The startup dump holds, in order:

- `0x02` (104 bytes), then the device information packet `0x1D` (257 bytes,
  [device-info.md](device-info.md));
- 21 settings of the `0x09` family, the noise control mode first, with `0x63` and `0x4E`
  among them ([settings.md](settings.md));
- `0x53`, `0x55`, and `0x17` three times.

With Conversation Awareness on, speech level `09` follows the dump, unasked (Pro 3,
2026-11-01, #150; [conversation-awareness.md](conversation-awareness.md)).

**On a new link the dump waits for the second `0x0C`**: on the Pro 3 in every #71
capture, and on the Pro 2 (USB-C) (#149). On a Pro 2 (USB-C) already connected when the
session opened, it came before that `0x0C` (#149). Until the dump comes, the noise
control mode and the pods' name are unknown. The battery and the ear reports do not
wait.

| Model | When the dump came | Source |
|---|---|---|
| Pro 3 | After the second `0x0C`, in every #71 capture | #71 captures 1, 2 and 4 |
| Not recorded; #109's other runs were on a Pro 3 | The mode came 20 s after the link, in a run where `0x0C` named another address for 18 s first | 2026-10-20, PR #109 |
| Pro 2 (USB-C) | After the second `0x0C`, on a new link | 2026-11-01, #149 |
| Pro 2 (USB-C) | 0.6 s *before* the second `0x0C`, with the pair already connected when the session opened | 2026-11-01, #149 |

The noise control mode in the dump: [noise-control.md](noise-control.md#initial-state-report).

### The key packet among the others

The key packet comes among the packets above, not first and not alone. A client looks at
each packet for the key packet's header and opcode, `0x31`, until one comes
([keys.md](keys.md#identifying-key-packets)).

## Opcodes

Every opcode in the runs cited in these documents, or read by the app, by byte 4: #71
captures 1, 2 and 4, #142, #149 and #150. "Both" means a client sends it and the pods
send it too.

| Opcode | From | What | Described in |
|---|---|---|---|
| `0x02` | Pods | The first packet of the startup dump, 104 bytes | [Not understood](#seen-but-not-understood) |
| `0x04` | Pods | Battery | [battery.md](battery.md) |
| `0x06` | Pods | Ear report: where each pod is | [ear.md](ear.md#ear-report-0x06) |
| `0x08` | Pods | Primary pod | [ear.md](ear.md#primary-pod-0x08) |
| `0x09` | Both | Settings, by sub-command at byte 6: the noise control mode (`0x0D`), Conversation Awareness (`0x28`), allow Off (`0x34`) and others | [settings.md](settings.md) |
| `0x0C` | Pods | Names a device: a pod, then this host | [Below](#0x0c-a-device-in-use) |
| `0x0E` | Pods | Names this host, with a type | [Below](#0x0e-the-audio-source) |
| `0x0F` | Client | Notification request | [Step 3](#step-3-send-the-requests) |
| `0x17` | Pods | Three at the end of the startup dump, and mid-link | [Not understood](#seen-but-not-understood) |
| `0x1A` | Client | Rename | [rename.md](rename.md#rename-0x1a) |
| `0x1D` | Pods | Device information: name, firmware version, serial numbers | [device-info.md](device-info.md) |
| `0x2B` | Pods | The hosts the pair knows | [Below](#0x2b-the-hosts-the-pair-knows) |
| `0x2E` | Pods | Names hosts, this one among them | [Below](#0x2e-hosts) |
| `0x30` | Client | Key request | [keys.md](keys.md#step-3-send-key-request) |
| `0x31` | Pods | Key packet: the IRK and the ENC_KEY | [keys.md](keys.md#key-packet-format) |
| `0x4B` | Pods | Conversation Awareness's speech level | [conversation-awareness.md](conversation-awareness.md#speech-levels-0x4b) |
| `0x4C` | Pods | Once mid-link, 11 bytes | [Not understood](#seen-but-not-understood) |
| `0x4D` | Client | Features | [Above](#prerequisites-the-features-packet) |
| `0x4E` | Pods | In the startup dump | [Not understood](#seen-but-not-understood) |
| `0x52` | Pods | Mid-link, 11 bytes | [Not understood](#seen-but-not-understood) |
| `0x53` | Pods | In the startup dump | [Not understood](#seen-but-not-understood) |
| `0x55` | Pods | In the startup dump, and mid-link | [Not understood](#seen-but-not-understood) |
| `0x57` | Pods | Mid-link, 15 bytes | [Not understood](#seen-but-not-understood) |
| `0x63` | Pods | In the startup dump | [Not understood](#seen-but-not-understood) |

Named elsewhere, never sent or seen here:

| Opcode | What | Source |
|---|---|---|
| `0x10` | Smart Routing: media information, the "Move to iPhone" banner, a takeover request | LibrePods' Android app (`AACPManager.kt`), #149 |
| `0x1E` | Rename, in LibrePods' own opcode table; both its apps send `0x1A` | [rename.md](rename.md#rename-0x1a) |
| `0x23` | Case Info, carrying the case's version | The dissector [device-info.md](device-info.md#fields) cites |

### `0x0C`: a device in use

```
04 00 04 00 0C 00 [address, 6 bytes, least significant first] [00|01] [01|02]
```

14 bytes (#71 captures). #71 capture 4 has three endings: `01 02`, `01 01` and `00 02`. What it means is not known: the
device the pods take to be in use, most likely - a guess.

- **At link-up, two of them.** The first names a pod, ending `01 02`. The second names
  this host, ending `01 01`, 1.4-1.5 s later (Pro 3, #71; Pro 2 (USB-C), 1.4 s, #149).
  The startup dump follows the second ([above](#initial-state-report)).
- **Which pod the first names:** the pair's own address in #71 capture 2 and in #149;
  in #71 capture 1 another address, which also stands in the `0x2B` list, most likely
  the other pod's. Not proven (#71).
- **It never named the host that held the pods before this one** (Pro 3, #71 capture 1),
  although the advertisement and the `0x2B` list named it.
- **At a lid change:** a `0x0C` came 1-2 ms before the `0x08`, ending `00 02` when the
  lid closed and `01 02` when it opened (Pro 3, #71 capture 4, as #115 records it).
- **LibrePods' Android app** calls it `AUDIO_SOURCE_2`, with the comment "seems
  redundant?" (#149).

### `0x0E`: the audio source

```
04 00 04 00 0E 00 [address, 6 bytes, least significant first] [type]
```

13 bytes, byte 5 `00` in all 12 of #71 capture 4.

- **Early on every link** it names this host, type `00` (`btmon`, 2026-10-20, #149;
  Pro 3, #71 captures; Pro 2 (USB-C), 2026-11-01, #149).
- **Type `02`** came once A2DP played to this host, 32 s into the link. Pausing sent no
  `0x0E` back to `00` (Pro 2 (USB-C), 2026-11-01, #149).
- **LibrePods' Android app** calls it `AUDIO_SOURCE`, and #149 reads it with it as the
  host that has the audio: type `00` none, `01` a call, `02` media.
- **It never named the iPhone** (#149). Its takeover dropped the link, the ACL
  included, with no packet before it (Pro 2 (USB-C), #149). On a Pro 3 the takeover
  showed as a connection reset by the pods (#71 capture 4).

### `0x2E`: hosts

```
04 00 04 00 2E 00 01 00 NN [entry] ...
entry:  [address, 6 bytes, most significant first] [2 bytes]
```

A count at byte 8, then 8 bytes per entry, the address the other way round from `0x0C`
and `0x0E` (Pro 3, #71 capture 4). What it means is not known.

- **17 bytes, one entry:** this host, then `02 02`. Twice at link-up (#71 captures 1, 2
  and 4), and beside an `0x0E` mid-link (#71 capture 4).
- **25 bytes, two entries:** once in #71 capture 4 (19:44:02 UTC): another address with
  `01 04`, then this host with `02 02`. The #142 logs, which kept 8 bytes of a packet,
  have three 25-byte `0x2E`s too.

It is not the `0x09` family's sub-command `0x2E` ([settings.md](settings.md)).

### `0x2B`: the hosts the pair knows

```
04 00 04 00 2B 00 01 [length, 2 bytes, little-endian] [8 bytes] [entries, 17 bytes each]
entry:  [8 bytes] [address, 6 bytes, least significant first] [3 bytes]
```

The layout is as the fake pods copy it, with made-up content (#148).

- **It comes unasked, with the handshake's answer.** In #71 capture 1 it came 3 µs after
  the answer, before the features packet was sent: so it does not answer the features
  packet, which nothing answers (Pro 3, #71).
- **Its entries** (Pro 3, #71): five in capture 1 (102 bytes) - the address the first
  `0x0C` named, this host, and the user's iPhone three times; four in capture 2 - the
  pair's own address, the address capture 1's first `0x0C` named, and this host twice.
- **Entries go:** the iPhone, its Bluetooth off, was no longer in the list in capture 2,
  although the pods had probably connected to it in between (#71).

### Seen but not understood

From a Pro 3: the startup dump of #71 captures 1 and 2, and the logs of #71 capture 4
and #142:

| Opcode | Seen | Named elsewhere |
|---|---|---|
| `0x02` | First in the dump, 104 bytes | - |
| `0x17` | Three times, last in the dump: 520, 572 and 28 bytes | Head tracking, in LibrePods' AAP Definitions |
| `0x4C` | Once in the #142 logs, 11 bytes | - |
| `0x4E` | Among the dump's settings, 14 bytes | - |
| `0x52` | Mid-link, 11 bytes, in #71 capture 4 and #142 | - |
| `0x53` | After the dump's settings, 140 bytes | Headphone Accommodation, in LibrePods' AAP Definitions |
| `0x55` | After `0x53`, 10 bytes | - |
| `0x57` | Mid-link, 15 bytes, in #71 capture 4 and #142 | - |
| `0x63` | Among the dump's settings, 13 bytes | - |

Mid-link in #71 capture 4, beside an `0x08`, the same group came again and again
(19:39:19-19:40:07 UTC): `0x55`, a `0x0C` ending `01 02`, a `0x0C` ending `01 01`,
`0x0E`, `0x2E`, and three `0x17`.

## Runs

### #71 capture 1 (Pro 3)

`btmon`, `dbus-monitor --system "sender='org.bluez'"`, and the app at
`RUST_LOG=linuxpods=trace`, build 06a426d, one adapter (posted 2026-10-25).

- Handshake answer after 1.23 s. In the same millisecond, before `0x4D` went out, a
  102-byte `0x2B`. The app then sent `0x0F`, `0x4D` and `0x30` within a millisecond.
- After `0x0F`: the battery at +146 ms; `08 00 02 00 01 00`; `0x0C` naming another
  address (`01 02`); `0x0E` naming this host; `0x2E` twice; `06 00 02 02`; the key
  packet at +350 ms.
- The second `0x0C`, naming this host (`01 01`), 1.4 s after the first; the startup dump
  57 ms after it, 1.56 s after `0x0F`.

### #71 capture 2 (Pro 3)

The same pair, tools and build, the pair blocked on this host until the connect (posted
2026-10-25).

- Handshake answer after 1.04 s, the same 18 bytes; `0x2B` 30 ms after it, four entries.
- After `0x0F`: the battery at +188 ms; `08 00 01 00`; `0x0C` naming the pair itself
  (`01 02`) at +270 ms; `0x0E` naming this host; `0x2E` twice and `06 00 02 02`; the key
  packet 385 ms after the request; the second `0x0C` (`01 01`) 1.46 s after the first.
- The startup dump 0.7 ms after that, 1.73 s after the answer: as in capture 1, but
  `0x38` = `52` (was `00`). No `0x34`, `0x1A` or `0x4B`.

### #71 capture 4 (Pro 3, 2026-11-01)

Build df29b14, `RUST_LOG=linuxpods=trace`. On all five connects nothing was dropped
ahead of the handshake's answer, and the order was: `0x2B`; the battery; `0x08`, then
`0x0C`, `0x0E` and `0x2E`; `0x06`, then `0x2E` again; the key packet; the second
`0x0C`, then the startup dump.

### #149 (Pro 2 (USB-C), 2026-11-01)

A local build logging `0x0E`, `0x0C` and any `0x09` sub-command `0x06` whole, over four
links: two idle, one playing on Linux, and one with the pair already connected when the
app started. The iPhone took the pods over three times. No `0x09 06` report came on any
link, the startup dump included.

## Open questions

- What `0x0C`, `0x0E`, `0x2E` and `0x2B` mean, field by field, and what `0x02`, `0x17`,
  `0x4E`, `0x53`, `0x55` and `0x63` carry.
- Whether the pods take a noise control command before the second `0x0C` (#71).
- What makes a host leave the `0x2B` list.
- What the other address in a two-entry `0x2E` is, and what `01 04` and `02 02` mean.
- What `0x4C`, `0x52` and `0x57` carry.
- Whether the notification request's `FF FF FE FF` and `FF FF FF FF` differ.
- What the features packet's bytes after `FF` select, and what fails without it.
