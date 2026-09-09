//! Probe: how many fresh-socket connect attempts does the handshake need?
//! Usage: cargo run --example aap_probe -- <MAC>

use std::time::{Duration, Instant};

use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType};

const PSM: u16 = 0x1001;
const HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mac = std::env::args().nth(1).expect("usage: aap_probe <MAC>");
    let addr: Address = mac.parse()?;
    let sa = SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM, cid: 0 };
    let t0 = Instant::now();

    for attempt in 1..=6 {
        let socket = Socket::<SeqPacket>::new_seq_packet()?;
        match socket.connect(sa).await {
            Ok(sock) => {
                let cid = sock.peer_addr().map(|a| a.cid).unwrap_or(0);
                match sock.send(&HANDSHAKE).await {
                    Ok(n) => {
                        println!(
                            "attempt {attempt}: OK - handshake {n} bytes, cid={cid}, total {:?}",
                            t0.elapsed()
                        );
                        return Ok(());
                    }
                    Err(e) => println!("attempt {attempt}: send failed (cid={cid}): {e}"),
                }
            }
            Err(e) => println!("attempt {attempt}: connect failed: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    println!("all attempts failed after {:?}", t0.elapsed());
    Ok(())
}
