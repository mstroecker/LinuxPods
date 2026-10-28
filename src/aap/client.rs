//! Apple Accessory Protocol client over L2CAP.
//!
//! bluer provides the AF_BLUETOOTH SOCK_SEQPACKET socket as a safe, typed, async
//! API, so no hand-laid `sockaddr_l2` or raw connect syscall is needed here.
//!
//! Under the `harness` feature a client can talk to a Unix SOCK_SEQPACKET socket
//! instead, for a fake device to answer (#74). It keeps message boundaries and
//! reports a closed peer as a zero-length read, as L2CAP does, so nothing past
//! [`Client::connect`] tells the two apart.
//!
//! Protocol flow, which [`Client::open`] runs:
//!   1. Open L2CAP connection to AirPods (PSM 4097)
//!   2. Send handshake, and wait for the device to answer it
//!   3. Request battery notifications and enable the special features
//!   4. Read incoming packets - `aap::decode` reads each one
//!
//! The proximity keys are not among the steps: the caller that stores them
//! asks, with [`Client::request_proximity_keys`] - see [`Client::open`].
//!
//! Based on reverse engineering from LibrePods and OpenPods.

use std::io;
#[cfg(feature = "harness")]
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType};
#[cfg(feature = "harness")]
use tokio_seqpacket::UnixSeqpacket;
use tracing::Instrument;

use crate::aap::{battery, keys, noise};

/// L2CAP Protocol/Service Multiplexer for AAP.
pub(crate) const AAP_PSM: u16 = 0x1001; // 4097

/// Max AAP packet we will read in one recv.
const READ_BUF_LEN: usize = 1024;

/// Connect attempts before giving up. The first attempt against an idle device
/// can lose the race with ACL link setup (see [`Client::connect`]): it did in
/// one #71 capture, and not in the other.
const CONNECT_ATTEMPTS: usize = 6;

/// Backoff between connect attempts.
const CONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Sent immediately after connecting to enable AAP communication.
const PACKET_HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// How the device's answer to the handshake starts. Pro 3 sends 18 bytes,
/// starting `01 00 04 00 00 00 01 00`; only the first four are matched, as
/// LibrePods matches them.
const HANDSHAKE_REPLY: [u8; 4] = [0x01, 0x00, 0x04, 0x00];

/// How long the device has to answer the handshake. A pair that had only just
/// connected took 1.2 s.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The span a link's log lines go under, naming the device: several links
/// can be up at once.
pub(crate) fn link_span(addr: Address) -> tracing::Span {
    tracing::info_span!("aap", mac = %addr)
}

/// What [`Client::read_packet`] fails with once the connection has closed:
/// the device hung up, or [`Client::shutdown`] closed it here.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("connection closed")]
pub(crate) struct Closed;

/// Whether `e` - from [`Client::read_packet`], say - is the connection going
/// away rather than something going wrong on it: closed from either end, or
/// taken down with the Bluetooth link, as an ordinary disconnect does - reset
/// by the device, aborted here, gone out of range, the adapter gone down.
pub(crate) fn is_link_gone(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.is::<Closed>()
            || cause.downcast_ref::<io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    io::ErrorKind::NotConnected
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::HostUnreachable
                        | io::ErrorKind::NetworkDown
                        | io::ErrorKind::NetworkUnreachable
                ) || e.raw_os_error() == Some(EHOSTDOWN)
            })
    })
}

/// Linux's "host is down", which `std` gives no kind of its own.
const EHOSTDOWN: i32 = 112;

/// What [`Client::connect`] fails with when the socket's connect does, and
/// the handshake when its send does - the first on the socket, which an L2CAP
/// connect can report up when it never came up: no connection came up. Its
/// message is the whole of what it says.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ConnectFailed(String);

/// Whether `e`, from bringing a link up - [`Client::connect`], the handshake,
/// the requests after it - is a connection that had come up going away, as
/// [`is_link_gone`] tells. A connect that fails is never that, whatever its
/// cause: nothing came up to go, and an L2CAP connect that times out or finds
/// the host down has failed.
pub(crate) fn went_during_setup(e: &anyhow::Error) -> bool {
    is_link_gone(e) && e.downcast_ref::<ConnectFailed>().is_none()
}

/// Where a fake listens for the app's AAP link to `addr`: `<dir>/<MAC>.sock`,
/// the MAC as [`Address`] displays it. One socket per pair keeps two fake pairs
/// apart, and a fake that binds by this same function cannot drift from it.
#[cfg(feature = "harness")]
pub(crate) fn unix_socket_path(dir: &Path, addr: Address) -> PathBuf {
    dir.join(format!("{addr}.sock"))
}

/// The socket a connected client talks over. Both kinds take `&self` for send
/// and recv, so a read parked in recv never blocks a send - `AapLink::run`
/// polls the read loop and the link's commands side by side in one task.
enum Transport {
    L2cap(SeqPacket),
    #[cfg(feature = "harness")]
    Unix(UnixSeqpacket),
}

impl Transport {
    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::L2cap(s) => s.send(buf).await,
            #[cfg(feature = "harness")]
            Self::Unix(s) => s.send(buf).await,
        }
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::L2cap(s) => s.recv(buf).await,
            #[cfg(feature = "harness")]
            Self::Unix(s) => s.recv(buf).await.map(|info| info.bytes_read()),
        }
    }

    fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        match self {
            Self::L2cap(s) => s.shutdown(how),
            #[cfg(feature = "harness")]
            Self::Unix(s) => s.shutdown(how),
        }
    }
}

/// An AAP client connected to a pair of AirPods.
pub struct Client {
    addr: Address,
    /// Where [`Client::connect`] goes in place of the device, under the harness.
    #[cfg(feature = "harness")]
    unix_path: Option<PathBuf>,
    socket: Option<Transport>,
}

impl Client {
    /// No I/O happens until [`Client::connect`].
    pub fn new(addr: Address) -> Self {
        Self {
            addr,
            #[cfg(feature = "harness")]
            unix_path: None,
            socket: None,
        }
    }

    /// A client for `addr` that connects to the Unix socket at `path`, where a
    /// fake answers for the device. No I/O happens until [`Client::connect`].
    #[cfg(feature = "harness")]
    pub(crate) fn with_unix_socket(addr: Address, path: PathBuf) -> Self {
        Self {
            unix_path: Some(path),
            ..Self::new(addr)
        }
    }

    /// Brings a session up on a client not yet connected, and returns it for
    /// its packets to be read: connects, sends the handshake and waits for
    /// the answer, then subscribes to battery notifications and enables the
    /// special features. What the device sends waits in the socket until the
    /// caller reads it.
    ///
    /// Nothing goes out before the handshake is answered - see
    /// [`Client::handshake`]. The proximity keys are not asked for here: the
    /// caller that stores them asks, with [`Client::request_proximity_keys`],
    /// so that a probe printing what comes back never receives them unasked.
    ///
    /// Fails as [`Client::connect`] and [`Client::handshake`] do, a link that
    /// came up and went told from one that never came up by
    /// [`went_during_setup`]. Dropped before it is done - timed out, say - it
    /// closes the connection along with the client.
    pub async fn open(mut self) -> Result<Self> {
        let span = link_span(self.addr);
        async move {
            self.connect().await?;
            // Back once the device has answered: anything sent sooner is dropped.
            self.handshake().await?;
            self.request_battery_status()
                .await
                .context("failed to request battery")?;
            self.enable_special_features()
                .await
                .context("failed to enable features")?;
            Ok(self)
        }
        .instrument(span)
        .await
    }

    /// Opens the L2CAP connection on PSM 4097 - or, for a client made with
    /// `with_unix_socket`, connects to that socket.
    pub async fn connect(&mut self) -> Result<()> {
        anyhow::ensure!(self.socket.is_none(), "already connected");

        // One attempt. The retries below are for L2CAP connect returning before
        // the ACL link is up, which a Unix socket never does.
        #[cfg(feature = "harness")]
        if let Some(path) = &self.unix_path {
            let socket = UnixSeqpacket::connect(path).await.with_context(|| {
                ConnectFailed(format!(
                    "failed to connect to AirPods at {} over {}",
                    self.addr,
                    path.display()
                ))
            })?;
            tracing::debug!("AAP connected to {} over {}", self.addr, path.display());
            self.socket = Some(Transport::Unix(socket));
            return Ok(());
        }

        let sa = SocketAddr {
            addr: self.addr,
            // AirPods use a classic (BR/EDR) address for AAP, matching the Go
            // version's bdaddr_type = 0.
            addr_type: AddressType::BrEdr,
            psm: AAP_PSM,
            cid: 0,
        };

        // The Go implementation used a blocking connect(2), which waits for the
        // BR/EDR ACL link to come up. bluer's socket is non-blocking and its
        // connect returns almost immediately (~30us) without waiting: when the ACL
        // link is not yet up the call still reports Ok, but the channel was never
        // established and every send fails with ENOTCONN. Such a socket stays dead
        // even after its cid later becomes nonzero, so it cannot be recovered.
        //
        // A zero cid immediately after connect is the reliable signal for that
        // case. Discard the socket, let the ACL link finish coming up, and retry
        // with a fresh one - which then connects with a real cid straight away.
        let mut last_err = None;

        for attempt in 1..=CONNECT_ATTEMPTS {
            let socket =
                Socket::<SeqPacket>::new_seq_packet().context("failed to create L2CAP socket")?;

            match socket.connect(sa).await {
                Ok(sock) => {
                    let cid = sock.peer_addr().map(|a| a.cid).unwrap_or(0);
                    if cid != 0 {
                        tracing::debug!(
                            "AAP connected to {} (cid {cid}, attempt {attempt})",
                            self.addr
                        );
                        self.socket = Some(Transport::L2cap(sock));
                        return Ok(());
                    }
                    tracing::debug!(
                        "AAP connect attempt {attempt}: channel not established (cid 0)"
                    );
                }
                Err(e) => {
                    tracing::debug!("AAP connect attempt {attempt} failed: {e}");
                    last_err = Some(e);
                }
            }

            if attempt < CONNECT_ATTEMPTS {
                tokio::time::sleep(CONNECT_BACKOFF).await;
            }
        }

        match last_err {
            Some(e) => Err(anyhow::Error::new(e).context(ConnectFailed(format!(
                "failed to connect to AirPods at {}",
                self.addr
            )))),
            None => anyhow::bail!(
                "failed to establish L2CAP channel to {} after {CONNECT_ATTEMPTS} attempts",
                self.addr
            ),
        }
    }

    /// Sends the handshake and waits for the device to answer it.
    ///
    /// The device drops what it is sent before it has answered. A pair that
    /// had only just connected answered 1.2 s after the handshake, and the
    /// requests that went out after a fixed 500 ms were never answered: no
    /// battery, no keys, for as long as the connection lasted.
    ///
    /// Fails when the device hangs up first, or has not answered within
    /// [`HANDSHAKE_TIMEOUT`].
    pub async fn handshake(&self) -> Result<()> {
        self.handshake_within(HANDSHAKE_TIMEOUT).await
    }

    async fn handshake_within(&self, limit: Duration) -> Result<()> {
        // The first send on the socket. One that fails is a connection that
        // never came up, not one that went: an L2CAP connect can report Ok
        // on a socket that is dead from the start - see `connect`.
        self.send_packet(&PACKET_HANDSHAKE, "handshake")
            .await
            .map_err(|e| e.context(ConnectFailed("the connection never came up".into())))?;
        let answered = async {
            loop {
                let packet = self.read_packet().await?;
                if packet.starts_with(&HANDSHAKE_REPLY) {
                    return anyhow::Ok(());
                }
                tracing::debug!(
                    "AAP packet ({} bytes) ahead of the handshake reply, dropped",
                    packet.len()
                );
            }
        };
        // Unanswered only past the limit. A read that fails first is the link
        // going, as when the pods reset it while handled (#123) - or, when
        // `is_link_gone` says otherwise, some other fault on it.
        tokio::time::timeout(limit, answered)
            .await
            .with_context(|| format!("handshake unanswered for {limit:?}"))?
            .map_err(|e| {
                let what = if is_link_gone(&e) {
                    "link went before the handshake was answered"
                } else {
                    "handshake failed before it was answered"
                };
                e.context(what)
            })
    }

    pub(crate) async fn request_battery_status(&self) -> Result<()> {
        self.send_packet(&battery::REQUEST, "battery request").await
    }

    pub(crate) async fn enable_special_features(&self) -> Result<()> {
        self.send_packet(&noise::ENABLE_FEATURES, "feature enable")
            .await
    }

    /// Requests the keys used to decrypt BLE proximity advertisements. The
    /// response arrives asynchronously and is handled by the read loop.
    pub async fn request_proximity_keys(&self) -> Result<()> {
        self.send_packet(&keys::REQUEST, "key request").await
    }

    /// Switches the noise control mode.
    ///
    /// Fire and forget: no answer confirms it on every model. AirPods 4 sent
    /// a generic settings-changed notification that names neither the
    /// sub-command nor the mode, and the 0x0D echo carrying the new mode only
    /// sometimes; AirPods Pro 3 echo every change, but about a second later.
    /// There is nothing here to wait for - the caller updates its own state
    /// optimistically.
    /// `enable_special_features` must have been sent first, or Adaptive is
    /// unavailable.
    pub async fn set_noise_mode(&self, mode: noise::NoiseMode) -> Result<()> {
        self.send_packet(&noise::set_packet(mode), "noise control")
            .await
    }

    /// Permits, or forbids, Off as a listening mode.
    ///
    /// Recent firmware refuses a bare Off command with an error chime; this is
    /// the setting that gates it. Persistent on the device and idempotent, so it
    /// is worth sending only when Off is actually wanted.
    pub async fn set_allow_off_listening_mode(&self, allowed: bool) -> Result<()> {
        self.send_packet(
            &noise::allow_off_packet(allowed),
            "allow off listening mode",
        )
        .await
    }

    /// Sends a packet, verifying the whole thing was written.
    async fn send_packet(&self, packet: &[u8], kind: &str) -> Result<()> {
        let socket = self.socket.as_ref().context("not connected")?;
        let n = socket
            .send(packet)
            .await
            .with_context(|| format!("failed to send {kind} ({} bytes)", packet.len()))?;
        anyhow::ensure!(
            n == packet.len(),
            "incomplete {kind} write: {n}/{} bytes",
            packet.len()
        );
        Ok(())
    }

    /// Reads a single AAP packet. Fails with [`Closed`] once the connection
    /// has closed.
    pub async fn read_packet(&self) -> Result<Vec<u8>> {
        let socket = self.socket.as_ref().context("not connected")?;
        let mut buf = vec![0u8; READ_BUF_LEN];
        let n = socket
            .recv(&mut buf)
            .await
            .context("failed to read packet")?;
        // SOCK_SEQPACKET reports a closed channel - the device hanging up, or our
        // own shutdown - as a zero-length read, not an error. An AAP packet is never
        // empty; returning one spun the read loop on a dead socket.
        if n == 0 {
            return Err(Closed.into());
        }
        buf.truncate(n);
        Ok(buf)
    }

    /// Shuts the socket down in both directions.
    ///
    /// Takes `&self` so it can be called through an `Arc` while a read loop is
    /// parked in `recv`; the shutdown is what wakes that read up with an error.
    pub(crate) fn shutdown(&self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    #[test]
    fn starts_unconnected() {
        let c = Client::new(MAC);
        assert_eq!(c.addr, MAC);
        assert!(c.socket.is_none());
    }

    /// The connection going away is told from a fault on it, under whatever
    /// context the error has gathered (#117).
    #[test]
    fn a_link_gone_is_told_from_a_fault() {
        let read = |e: anyhow::Error| e.context("failed to read packet");
        assert!(is_link_gone(&read(Closed.into())));
        for kind in [
            io::ErrorKind::NotConnected,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
            io::ErrorKind::NetworkDown,
        ] {
            assert!(
                is_link_gone(&read(io::Error::from(kind).into())),
                "{kind:?}"
            );
        }
        let host_down = io::Error::from_raw_os_error(EHOSTDOWN);
        assert!(is_link_gone(&read(host_down.into())));
        assert!(!is_link_gone(&read(
            io::Error::from(io::ErrorKind::PermissionDenied).into()
        )));
        assert!(!is_link_gone(&anyhow::anyhow!("connection closed")));
    }

    /// A link that came up and went during setup is told from a connect that
    /// failed, though both can carry the same error: an L2CAP connect timing
    /// out, or finding the host down, failed.
    #[test]
    fn a_link_gone_during_setup_is_told_from_a_failed_connect() {
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        let handshake = anyhow::Error::new(reset)
            .context("failed to read packet")
            .context("link went before the handshake was answered");
        assert!(went_during_setup(&handshake));
        // As attachment gets it, under its own context.
        assert!(went_during_setup(&handshake.context("setting up")));

        for connect in [
            io::Error::from(io::ErrorKind::TimedOut),
            io::Error::from_raw_os_error(EHOSTDOWN),
        ] {
            let failed = anyhow::Error::new(connect)
                .context(ConnectFailed("failed to connect to AirPods".into()));
            assert!(is_link_gone(&failed));
            assert!(!went_during_setup(&failed), "{failed:#}");
        }
        let unanswered = anyhow::anyhow!("handshake unanswered for 5s");
        assert!(!went_during_setup(&unanswered));
    }

    #[tokio::test]
    async fn send_and_read_require_connection() {
        let c = Client::new(MAC);
        assert!(c.handshake().await.is_err());
        assert!(c.read_packet().await.is_err());
    }

    /// Over a Unix socket, as the harness runs AAP.
    #[cfg(feature = "harness")]
    mod unix_socket {
        use std::sync::Arc;

        use tokio::time::timeout;
        use tokio_seqpacket::UnixSeqpacketListener;

        use super::*;

        /// Plenty for any of these, and a hang fails the test instead of CI.
        const TIMEOUT: Duration = Duration::from_secs(5);

        /// A client connected to a listener in a temporary directory, and the
        /// listener's end of the connection - the fake's.
        async fn connected() -> (Client, UnixSeqpacket, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let path = unix_socket_path(dir.path(), MAC);
            let mut listener = UnixSeqpacketListener::bind(&path).unwrap();
            let mut client = Client::with_unix_socket(MAC, path);
            client.connect().await.unwrap();
            let peer = listener.accept().await.unwrap();
            assert!(client.socket.is_some());
            (client, peer, dir)
        }

        /// The next message the peer receives, whole.
        async fn recv(peer: &UnixSeqpacket) -> Vec<u8> {
            let mut buf = [0u8; READ_BUF_LEN];
            let info = timeout(TIMEOUT, peer.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            buf[..info.bytes_read()].to_vec()
        }

        async fn next_packet(client: &Client) -> Result<Vec<u8>> {
            timeout(TIMEOUT, client.read_packet()).await.unwrap()
        }

        #[test]
        fn the_socket_is_named_after_the_mac() {
            assert_eq!(
                unix_socket_path(Path::new("/run/fake"), MAC),
                Path::new("/run/fake/AA:BB:CC:DD:EE:FF.sock")
            );
        }

        /// Each packet is one message, never two run together nor one split:
        /// the parsers take whole packets, as L2CAP delivers them.
        #[tokio::test]
        async fn packets_keep_their_boundaries_in_both_directions() {
            let (client, peer, _dir) = connected().await;

            client.request_battery_status().await.unwrap();
            client.enable_special_features().await.unwrap();
            assert_eq!(recv(&peer).await, battery::REQUEST);
            assert_eq!(recv(&peer).await, noise::ENABLE_FEATURES);

            // Left at 83%, discharging.
            let battery = [
                0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x04, 0x01, 0x53, 0x02, 0x01,
            ];
            peer.send(&battery).await.unwrap();
            peer.send(&[0x04, 0x00]).await.unwrap();
            assert_eq!(next_packet(&client).await.unwrap(), battery);
            assert_eq!(next_packet(&client).await.unwrap(), [0x04, 0x00]);
        }

        /// The handshake is done once the device answers it, and not before:
        /// whatever comes ahead of the answer is not it.
        #[tokio::test]
        async fn the_handshake_waits_for_the_answer() {
            let (client, peer, _dir) = connected().await;
            let client = Arc::new(client);
            let handshake = tokio::spawn({
                let client = client.clone();
                async move { client.handshake().await }
            });
            assert_eq!(recv(&peer).await, PACKET_HANDSHAKE);

            peer.send(&[0x04, 0x00, 0x04, 0x00, 0x2B, 0x00])
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!handshake.is_finished(), "not answered yet");

            // As Pro 3 starts its answer.
            peer.send(&[0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00])
                .await
                .unwrap();
            timeout(TIMEOUT, handshake).await.unwrap().unwrap().unwrap();
        }

        /// A session is the handshake, then - once it is answered, and not
        /// before - the battery subscription and the features, in that
        /// order. Not the key request: that is for the caller storing keys.
        #[tokio::test]
        async fn open_requests_once_the_handshake_is_answered() {
            let dir = tempfile::tempdir().unwrap();
            let path = unix_socket_path(dir.path(), MAC);
            let mut listener = UnixSeqpacketListener::bind(&path).unwrap();
            let open = tokio::spawn(Client::with_unix_socket(MAC, path).open());
            let peer = timeout(TIMEOUT, listener.accept()).await.unwrap().unwrap();

            assert_eq!(recv(&peer).await, PACKET_HANDSHAKE);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!open.is_finished(), "not answered yet");
            peer.send(&HANDSHAKE_REPLY).await.unwrap();

            assert_eq!(recv(&peer).await, battery::REQUEST);
            assert_eq!(recv(&peer).await, noise::ENABLE_FEATURES);
            let client = timeout(TIMEOUT, open).await.unwrap().unwrap().unwrap();
            assert!(client.socket.is_some());
            let mut buf = [0u8; READ_BUF_LEN];
            let more = timeout(Duration::from_millis(50), peer.recv(&mut buf)).await;
            assert!(more.is_err(), "sent more: {more:?}");
        }

        /// A device that hangs up before answering fails the session as the
        /// handshake does: a link that went during setup.
        #[tokio::test]
        async fn open_fails_when_the_device_hangs_up_first() {
            let dir = tempfile::tempdir().unwrap();
            let path = unix_socket_path(dir.path(), MAC);
            let mut listener = UnixSeqpacketListener::bind(&path).unwrap();
            let open = tokio::spawn(Client::with_unix_socket(MAC, path).open());
            let peer = timeout(TIMEOUT, listener.accept()).await.unwrap().unwrap();
            assert_eq!(recv(&peer).await, PACKET_HANDSHAKE);
            drop(peer);
            let failed = timeout(TIMEOUT, open).await.unwrap().unwrap();
            let failed = failed.err().expect("the session fails");
            assert!(went_during_setup(&failed), "{failed:#}");
        }

        /// A device that never answers fails the handshake rather than hold up
        /// the connection.
        #[tokio::test]
        async fn an_unanswered_handshake_fails() {
            let (client, _peer, _dir) = connected().await;
            let unanswered = client.handshake_within(Duration::from_millis(50)).await;
            let unanswered = unanswered.unwrap_err();
            assert_eq!(unanswered.to_string(), "handshake unanswered for 50ms");
            assert!(!went_during_setup(&unanswered));
        }

        /// A device that hangs up before it answers has not left the
        /// handshake unanswered: the link went (#123).
        #[tokio::test]
        async fn hanging_up_before_answering_fails_the_handshake() {
            let (client, peer, _dir) = connected().await;
            let handshake = tokio::spawn(async move { client.handshake().await });
            assert_eq!(recv(&peer).await, PACKET_HANDSHAKE);
            drop(peer);
            let failed = timeout(TIMEOUT, handshake).await.unwrap().unwrap();
            let failed = failed.unwrap_err();
            assert_eq!(
                format!("{failed:#}"),
                "link went before the handshake was answered: connection closed"
            );
            assert!(went_during_setup(&failed));
        }

        /// A connection gone before the handshake could even be sent never
        /// came up, as a socket L2CAP reports connected while the link is not
        /// up: a failed connect, not a link that went.
        #[tokio::test]
        async fn a_handshake_that_cannot_be_sent_is_a_failed_connect() {
            let (client, peer, _dir) = connected().await;
            drop(peer);
            let failed = client.handshake().await.unwrap_err();
            assert!(
                failed.downcast_ref::<ConnectFailed>().is_some(),
                "{failed:#}"
            );
            assert!(!went_during_setup(&failed), "{failed:#}");
        }

        /// What closing a link relies on: its read loop is parked in recv, and
        /// only the shutdown gets it out.
        #[tokio::test]
        async fn shutdown_wakes_a_parked_read() {
            let (client, _peer, _dir) = connected().await;
            let client = Arc::new(client);
            let parked = tokio::spawn({
                let client = client.clone();
                async move { client.read_packet().await }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!parked.is_finished(), "nothing to read yet");

            client.shutdown();
            let read = timeout(TIMEOUT, parked).await.unwrap().unwrap();
            assert_eq!(read.unwrap_err().to_string(), "connection closed");
        }

        /// A fake hanging up reads as the device would: a closed connection,
        /// not an empty packet for the read loop to spin on.
        #[tokio::test]
        async fn a_peer_hanging_up_closes_the_connection() {
            let (client, peer, _dir) = connected().await;
            drop(peer);
            let read = next_packet(&client).await;
            assert_eq!(read.unwrap_err().to_string(), "connection closed");
        }

        /// Nothing listening is a failed connection, not a fall back to L2CAP.
        #[tokio::test]
        async fn connecting_without_a_listener_fails() {
            let dir = tempfile::tempdir().unwrap();
            let mut client = Client::with_unix_socket(MAC, unix_socket_path(dir.path(), MAC));
            let failed = client.connect().await.unwrap_err();
            assert!(
                failed.downcast_ref::<ConnectFailed>().is_some(),
                "{failed:#}"
            );
            assert!(client.socket.is_none());
        }
    }
}
