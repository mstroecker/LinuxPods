# L2CAP

What the kernel's L2CAP sockets, and bluer's connect on top of them, do on a connect
that an AAP client has to know. Read from the source of Linux 7.2 and bluer 0.17.4, and
checked on hardware where a run below says so (Pro 3, 2026-10-28, #136; Pro 3 and Pro 2
(USB-C), 2026-10-29, PR #137).

The AAP client's connect is in `aap::client`; how the app waits for it:
[app/aap-links.md](../../app/aap-links.md#step-1-connect-to-airpods).

## Connecting

⚠️ **Never drop an L2CAP socket while its connect is pending.** A zero `cid` after
bluer's connect means the connect has not finished, not that the socket is dead. Read as
a dead socket and retried, it left the pods holding a channel this end had forgotten,
and that orphan later closed the live link (#136).

**A connect pages the device.** An L2CAP connect to a device whose ACL link is down
brings the link back up: on BR/EDR, `l2cap_chan_connect` calls `hci_connect_acl`, which
creates the ACL link when there is none (Linux 7.2, `net/bluetooth/l2cap_core.c` and
`net/bluetooth/hci_conn.c`; 2e69ed8, #49). A connect to a pair the user has just
disconnected reconnects it.

**bluer's connect can return before the channel is up.** In bluer 0.17.4
(`src/sock.rs`, `connect_priv`), upstream as
[bluez/bluer#163](https://github.com/bluez/bluer/issues/163) (#136):

1. After `EINPROGRESS`, it waits for the socket to poll writable, then reads `SO_ERROR`.
2. A new L2CAP socket polls writable already in `BT_OPEN`: `bt_sock_poll` holds back
   `EPOLLOUT` only in `BT_CONNECT`, `BT_CONNECT2` and `BT_CONFIG` (Linux 7.2,
   `net/bluetooth/af_bluetooth.c`; #136).
3. So `SO_ERROR` can read 0 with the channel still in `BT_CONNECT`, and the connect
   returns `Ok`.

**How a pending channel looks:**

| State | What the socket shows |
|---|---|
| Connecting (`BT_CONNECT`) | A zero `cid` in the peer address (bluer's `peer_addr().cid`). Every send fails `ENOTCONN`: `l2cap_sock_sendmsg` refuses anything short of `BT_CONNECTED` (Linux 7.2, `net/bluetooth/l2cap_sock.c`; 2cc3672). |
| Up (`BT_CONNECTED`) | `L2CAP_CONNINFO` (bluer's `conn_info()`) reads back (2cc3672, #136). |
| Closed | `getpeername` (bluer's `peer_addr()`) fails `ENOTCONN`; the reason is in `SO_ERROR` (2cc3672, #136). |

**The kernel gives up a connect after 40 s.** A new L2CAP socket's send timeout is
`L2CAP_CONN_TIMEOUT`, 40 s (`include/net/bluetooth/l2cap.h`, set in
`net/bluetooth/l2cap_sock.c`), and `l2cap_chan_connect` arms the channel's timer with it
(`net/bluetooth/l2cap_core.c`; all Linux 7.2; 2cc3672, #136).

## Closing a pending channel

- **The device is not told.** A socket closed while its channel is in `BT_CONNECT` has
  the channel deleted on this end, and no Disconnection Request goes out
  (`l2cap_chan_close` deletes a channel in `BT_CONNECT` without
  `l2cap_send_disconn_req`; Linux 7.2, `net/bluetooth/l2cap_core.c`; #136). The device
  goes on and completes the channel (btmon, 2026-10-28, #136).
- **The next connect gets the same local channel.** Both Connection Requests in the run
  below used local channel 64, so the device held two channels to one channel of ours
  (#136).
- **The device keeps the session on the first channel.** A Pro 3 left the handshake on
  the second channel unanswered (#136).
- **Closing the orphan closes the live link.** When the device closed the orphan, the
  kernel found the channel by the local channel number alone and closed the link that
  used it (#136).

## Runs

**The orphan channel: Pro 3, 2026-10-28, a791646, with `btmon` (#136).** Every AAP
connect took two attempts: the first returned `cid 0` and was dropped, the second
worked.

| Time | What happened |
|---|---|
| 21:21:24.44 | Attempt 1: Connection Request, local channel 64. The app saw `cid 0` and dropped the socket. No Disconnection Request went out. |
| 21:21:24.45 | Attempt 2: Connection Request, local channel 64 again. |
| 21:21:24.8 | The pods answered both: attempt 1 got remote channel 4357, attempt 2 got 4614. Both answers were "pending", then "successful". |
| 21:21:24.88 | The handshake went out on 4614. It was never answered: the pods kept the session on 4357. |
| 21:21:29.88 | No answer for 5 s. 4614 was closed. |
| 21:21:31 | Link 2 (one attempt, remote 4870) worked: battery, settings dump, keys. |
| 21:21:34.83 | The pods closed the orphan: Disconnection Request, destination 64, source 4357. The kernel closed link 2; its response named 4870. |
| 21:21:36.84 | Link 3: attempt 1 again got `cid 0` and was orphaned (remote 5131); attempt 2 got 5387. A Configure Request for one of the old channels was rejected. |
| 21:21:37.25 | The handshake on 5387 was answered. Then came a packet no earlier capture had, and nothing after it was answered. |

**The pending connect waited for: Pro 3 and Pro 2 (USB-C), 2026-10-29, 8bc90ba, kernel
7.2.11, with `btmon` and `dbus-monitor`, `RUST_LOG=linuxpods=trace` (PR #137).**

- 19 AAP connects, 19 L2CAP Connection Requests on PSM 4097: one per link.
- 13 of the 19 returned with `cid 0`. Each channel came up 133-567 ms later.
- All 19 came up on the first attempt, in 133-673 ms. No channel was left behind, no
  handshake went unanswered, and no link closed on its own.

```
AAP connect to <MAC> returned with the channel still connecting (cid 0); waiting for it
AAP channel to <MAC> came up after 463 ms pending
AAP connected to <MAC> (cid 21254) in 463 ms
```

## Open questions

- How the socket's writable readiness reaches tokio before `connect()` is called: a
  hypothesis, not confirmed (#136).
