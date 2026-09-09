//! Decrypts captured advertisements with the stored key, bypassing validation,
//! so the plaintext can be inspected directly.
//!
//! Usage: cargo run --example decrypt_probe

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};
use linuxpods::keystore::Keystore;

/// Payloads captured from the running app (AirPods Pro 3, model 0x2720).
const CAPTURED: &[&str] = &[
    "01 27 20 05 66 f3 51 00 00 3e e1 09 50 3d e9 16 6e 44 f2 82 f3 d3 82 28 95",
    "01 27 20 05 66 f3 51 00 00 fd b8 95 53 23 c3 b2 a0 05 55 2b 03 f4 e6 60 af",
    "01 27 20 15 66 f3 51 00 00 2f 2b 63 7b 12 04 ae 03 71 ce e7 92 95 5a 97 3d",
    "01 27 20 75 66 f3 51 00 00 62 84 ee 3c 78 44 99 cc 44 6e 72 78 f1 c4 34 40",
    // Captured later, same device while charging (70/70/50):
    "01 27 20 14 77 f5 59 00 00 12 d1 88 e9 8b bd c8 6f 14 78 ed cd 5e 52 7d c7",
];

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ks = Keystore::new()?;
    let keys = ks.load()?;
    println!("stored keys: {:?}\n", keys.keys().collect::<Vec<_>>());

    for (mac, key) in &keys {
        println!("=== key for {mac} ({} bytes) ===", key.len());
        let cipher = Aes128::new_from_slice(key)?;

        for raw in CAPTURED {
            let payload = hex(raw);
            let ct = &payload[9..25];

            let mut block = Block::<Aes128>::try_from(ct)?;
            cipher.decrypt_block(&mut block);
            let pt = block.as_slice();

            let magic_ok = (pt[0] & 0xF0) == 0 && pt[4] == 0x2D;
            println!(
                "  ct[0..4]={:02x?} -> pt={} | byte0={:02x} byte4={:02x} magic={}",
                &ct[..4],
                pt.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(""),
                pt[0],
                pt[4],
                if magic_ok { "OK" } else { "FAIL" }
            );
            println!(
                "     as batteries: b1={} b2={} b3={} (valid if <=100 after masking bit7)",
                pt[1] & 0x7F,
                pt[2] & 0x7F,
                pt[3] & 0x7F
            );
        }
        println!();
    }
    Ok(())
}
