//! Decrypts captured advertisements with the stored key, bypassing validation,
//! so the plaintext can be inspected directly.
//!
//! Usage: cargo run --example decrypt_probe

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};
use linuxpods::keystore::Keystore;

/// Payloads captured from the running app (AirPods Pro 3, model 0x2720).
const CAPTURED: &[&str] = &[
    // AirPods Pro 3 (model 0x2720)
    "01 27 20 05 66 f3 51 00 00 3e e1 09 50 3d e9 16 6e 44 f2 82 f3 d3 82 28 95",
    "01 27 20 14 77 f5 59 00 00 12 d1 88 e9 8b bd c8 6f 14 78 ed cd 5e 52 7d c7",
    // AirPods Pro Gen 2 (model 0x2420)
    "01 24 20 25 aa f1 51 00 00 46 0e 45 fc cf ae 4e ff f6 70 ff 14 46 bd 19 ff",
    "01 24 20 25 aa f1 51 00 00 4f cb 61 56 ef 4a f9 64 bd 4d f2 e5 f1 e2 58 c0",
    "01 24 20 34 aa f1 51 00 00 92 f3 36 1d bd da 42 5c b3 2e da 0c 35 b2 0a 7f",
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
            let suffix: Vec<u8> = mac.split(':').skip(3)
                .map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
            if pt[7..10] != suffix[..] { continue; }
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
