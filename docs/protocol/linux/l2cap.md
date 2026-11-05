# L2CAP

What the kernel's L2CAP sockets do on a connect that an AAP client has to know.

## Connecting

⚠️ bluer's connect can report success with the channel still connecting: a zero
`peer_addr().cid` means the connect has not finished, and every send fails with
`ENOTCONN` until it has. **Wait for the channel, and never drop the socket while it is
pending**: `conn_info()` reads back once the channel is up, and `peer_addr()` fails
`ENOTCONN` once it has closed. A socket closed while connecting leaves the device
holding a channel this end has forgotten, and a retry that gets the same local channel
shares it with that orphan (#136).

How the app waits: [app/aap-links.md](../../app/aap-links.md#step-1-connect-to-airpods).
