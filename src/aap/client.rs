//! Apple Accessory Protocol client over L2CAP.
//!
//! bluer provides the AF_BLUETOOTH SOCK_SEQPACKET socket as a safe, typed, async
//! API, so no hand-laid `sockaddr_l2` or raw connect syscall is needed here.
//!
//! Protocol flow:
//!   1. Open L2CAP connection to AirPods (PSM 4097)
//!   2. Send handshake
//!   3. Request battery notifications
//!   4. Parse incoming packets
//!
//! Based on reverse engineering from LibrePods and OpenPods.

use std::time::Duration;

use anyhow::{Context, Result};
use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType};

use crate::aap::noise;

/// L2CAP Protocol/Service Multiplexer for AAP.
pub const AAP_PSM: u16 = 0x1001; // 4097

/// Max AAP packet we will read in one recv.
const READ_BUF_LEN: usize = 1024;

/// Connect attempts before giving up. The first attempt against an idle device
/// usually loses the race with ACL link setup (see [`Client::connect`]).
const CONNECT_ATTEMPTS: usize = 6;

/// Backoff between connect attempts.
const CONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Sent immediately after connecting to enable AAP communication.
const PACKET_HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Subscribes to battery status notifications.
const PACKET_BATTERY_REQUEST: [u8; 10] =
    [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];

/// Enables conversational awareness and adaptive transparency.
const PACKET_ENABLE_FEATURES: [u8; 14] = [
    0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Requests the proximity pairing encryption keys (IRK and ENC_KEY).
const PACKET_KEY_REQUEST: [u8; 8] = [0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00];

/// An AAP client connected to a pair of AirPods.
pub struct Client {
    addr: Address,
    socket: Option<SeqPacket>,
}

impl Client {
    /// Parses the MAC address; no I/O happens until [`Client::connect`].
    pub fn new(mac_addr: &str) -> Result<Self> {
        let addr: Address = mac_addr
            .parse()
            .with_context(|| format!("invalid MAC address: {mac_addr}"))?;
        Ok(Self { addr, socket: None })
    }

    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    /// Opens the L2CAP connection on PSM 4097.
    pub async fn connect(&mut self) -> Result<()> {
        anyhow::ensure!(self.socket.is_none(), "already connected");

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
                        self.socket = Some(sock);
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
            Some(e) => Err(anyhow::Error::new(e)
                .context(format!("failed to connect to AirPods at {}", self.addr))),
            None => anyhow::bail!(
                "failed to establish L2CAP channel to {} after {CONNECT_ATTEMPTS} attempts",
                self.addr
            ),
        }
    }

    pub async fn handshake(&self) -> Result<()> {
        self.send_packet(&PACKET_HANDSHAKE, "handshake").await
    }

    pub async fn request_battery_status(&self) -> Result<()> {
        self.send_packet(&PACKET_BATTERY_REQUEST, "battery request")
            .await
    }

    pub async fn enable_special_features(&self) -> Result<()> {
        self.send_packet(&PACKET_ENABLE_FEATURES, "feature enable")
            .await
    }

    /// Requests the keys used to decrypt BLE proximity advertisements. The
    /// response arrives asynchronously and is handled by the read loop.
    pub async fn request_proximity_keys(&self) -> Result<()> {
        self.send_packet(&PACKET_KEY_REQUEST, "key request").await
    }

    /// Switches the noise control mode.
    ///
    /// Fire and forget: the device answers with a generic settings-changed
    /// notification that names neither the sub-command nor the mode, and the
    /// 0x0D echo carrying the new mode arrives only sometimes. There is nothing
    /// here to wait for - the caller updates its own state optimistically.
    /// `enable_special_features` must have been sent first, or Adaptive is
    /// unavailable.
    pub async fn set_noise_mode(&self, mode: noise::NoiseMode) -> Result<()> {
        self.send_packet(&noise::set_packet(mode), "noise control")
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

    /// Reads a single AAP packet.
    pub async fn read_packet(&self) -> Result<Vec<u8>> {
        let socket = self.socket.as_ref().context("not connected")?;
        let mut buf = vec![0u8; READ_BUF_LEN];
        let n = socket
            .recv(&mut buf)
            .await
            .context("failed to read packet")?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Shuts the socket down in both directions.
    ///
    /// Takes `&self` so it can be called through an `Arc` while a read loop is
    /// parked in `recv`; the shutdown is what wakes that read up with an error.
    pub fn shutdown(&self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Drops the socket, closing the connection.
    pub fn close(&mut self) {
        self.socket = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_mac() {
        let c = Client::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(c.addr.to_string(), "AA:BB:CC:DD:EE:FF");
        assert!(!c.is_connected());
    }

    #[test]
    fn rejects_invalid_mac() {
        assert!(Client::new("not-a-mac").is_err());
        assert!(Client::new("AA:BB:CC:DD:EE").is_err());
    }

    #[tokio::test]
    async fn send_and_read_require_connection() {
        let c = Client::new("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(c.handshake().await.is_err());
        assert!(c.read_packet().await.is_err());
    }
}
