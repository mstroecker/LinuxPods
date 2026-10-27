//! Asks a pair of AirPods for their proximity keys, and shows what comes back:
//! a debugger for a key request that fails, on a new model say.
//!
//! It opens the session the app opens, sends the key request, and lists every
//! packet that arrives for a few seconds - its opcode, its length and what it
//! is. A key packet (opcode 0x31) is laid out key by key, type and length as
//! the packet gives them, followed by whether the app would take its ENC_KEY.
//! The key bytes themselves are never printed: the output is meant to be
//! pasted into an issue, and the keys are the pair's IRK and ENC_KEY.
//!
//! Nothing is stored. The app asks for the keys on every connection and keeps
//! them itself; a debugging run must not replace a key that works.
//!
//! Usage: cargo run --example key_request -- <MAC> [SECONDS]
//!
//! SECONDS is how long to listen after the request, 5 by default.

use std::time::Duration;

use anyhow::Context;
use bluer::Address;
use linuxpods::aap::keys::{self, KeyType};
use linuxpods::aap::message::opcode;
use linuxpods::aap::{Client, Message, decode, find_encryption_key, parse_proximity_keys};

/// How long to listen after the request, unless the command line says.
const DEFAULT_WINDOW: Duration = Duration::from_secs(5);

/// Says what a packet is, as far as the app reads it.
fn describe(packet: &[u8]) -> String {
    match decode(packet) {
        Ok(Message::Battery(_)) => "battery".to_string(),
        Ok(Message::NoiseMode(mode)) => format!("noise control mode report: {mode}"),
        Ok(Message::Keys(keys)) => format!("proximity keys, {} of them", keys.len()),
        Ok(Message::Unknown { opcode: Some(_) }) => "not read by the app".to_string(),
        Ok(Message::Unknown { opcode: None }) => "no AAP header".to_string(),
        Err(e) => format!("unparsable: {e}"),
    }
}

/// A packet's opcode as printed: `0x31`, or `--` for a packet without the AAP
/// header.
fn opcode_text(packet: &[u8]) -> String {
    opcode(packet).map_or_else(|| "--".to_string(), |op| format!("0x{op:02X}"))
}

/// A key packet laid out a line at a time, as the packet gives it - whether or
/// not it parses - and then whether the app would take its ENC_KEY. Every byte
/// but the key data is shown; of the key data, only its length.
fn key_packet_layout(packet: &[u8]) -> Vec<String> {
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut lines = vec![format!("total length: {} bytes", packet.len())];
    if packet.len() < keys::PREAMBLE_LEN {
        lines.push(format!(
            "preamble cut short: {} (header, opcode, byte 5, key count)",
            hex(packet)
        ));
    } else {
        lines.push(format!(
            "header {}, opcode 0x{:02x}, byte 5 0x{:02x}, key count {}",
            hex(&packet[..4]),
            packet[4],
            packet[5],
            packet[6]
        ));
        let mut offset = keys::PREAMBLE_LEN;
        for n in 1..=packet[6] {
            if offset + 4 > packet.len() {
                lines.push(format!(
                    "key {n}: header cut short at byte {offset}, {} bytes left",
                    packet.len() - offset
                ));
                break;
            }
            let header = &packet[offset..offset + 4];
            let declared = usize::from(header[2]);
            let left = packet.len() - (offset + 4);
            let length = if declared <= left {
                format!("{declared} bytes")
            } else {
                format!("{declared} bytes declared, {left} left in the packet")
            };
            lines.push(format!(
                "key {n} at byte {offset}: type 0x{:02x} {}, length {length} \
                 [header {}] (data redacted)",
                header[0],
                KeyType::from(header[0]),
                hex(header)
            ));
            offset += 4 + declared;
        }
        if offset < packet.len() {
            lines.push(format!(
                "{} bytes after the last key (redacted)",
                packet.len() - offset
            ));
        }
    }
    lines.push(
        match parse_proximity_keys(packet).and_then(|keys| find_encryption_key(&keys)) {
            Ok(key) => format!(
                "the app would take this ENC_KEY: {} bytes, as it must be",
                key.len()
            ),
            Err(e) => format!("the app would not take this packet: {e}"),
        },
    );
    lines
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let arg = args.next().expect("usage: key_request <MAC> [SECONDS]");
    let mac: Address = arg
        .parse()
        .with_context(|| format!("not a MAC address: {arg}"))?;
    let window = match args.next() {
        Some(secs) => Duration::from_secs(
            secs.parse()
                .with_context(|| format!("not a number of seconds: {secs}"))?,
        ),
        None => DEFAULT_WINDOW,
    };

    println!("connecting AAP to {mac}...");
    // The session the app opens, which asks for the keys only after it.
    let client = Client::new(mac).open().await?;
    println!("connected");
    client.request_proximity_keys().await?;
    println!(
        "key request sent: {}; listening for {}s\n",
        keys::redacted_hex(&keys::REQUEST),
        window.as_secs()
    );

    let deadline = tokio::time::Instant::now() + window;
    let mut arrived = Vec::new();
    let mut key_packets = 0;
    while let Ok(read) = tokio::time::timeout_at(deadline, client.read_packet()).await {
        let packet = match read {
            Ok(packet) => packet,
            Err(e) => {
                println!("read failed: {e:#}");
                break;
            }
        };
        let summary = format!(
            "opcode {} ({} bytes) - {}",
            opcode_text(&packet),
            packet.len(),
            describe(&packet)
        );
        println!("{summary}");
        if opcode(&packet) == Some(keys::OPCODE) {
            key_packets += 1;
            for line in key_packet_layout(&packet) {
                println!("    {line}");
            }
        }
        arrived.push(summary);
    }

    if key_packets == 0 {
        println!(
            "\nno key packet (opcode 0x{:02X}) arrived within {}s",
            keys::OPCODE,
            window.as_secs()
        );
        if arrived.is_empty() {
            println!("nothing arrived at all");
        } else {
            println!("what did arrive, {} packets:", arrived.len());
            for summary in &arrived {
                println!("  {summary}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key packet as the device sends one, an IRK then an ENC_KEY, from
    /// synthetic keys: the shape in `docs/aap-key-retrieval.md`.
    fn key_packet(enc_len: u8) -> Vec<u8> {
        let mut packet = vec![0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x02];
        packet.extend_from_slice(&[0x01, 0x00, 0x10, 0x00]);
        packet.extend_from_slice(&[0xAA; 16]);
        packet.extend_from_slice(&[0x04, 0x00, enc_len, 0x00]);
        packet.extend(std::iter::repeat_n(0xBB, enc_len.into()));
        packet
    }

    /// No key byte goes out: every line is free of them.
    fn assert_redacted(lines: &[String]) {
        for line in lines {
            assert!(!line.contains("aa") && !line.contains("bb"), "{line}");
        }
    }

    #[test]
    fn a_key_packet_is_laid_out_key_by_key_without_its_keys() {
        let lines = key_packet_layout(&key_packet(16));
        assert_eq!(
            lines,
            [
                "total length: 47 bytes",
                "header 04 00 04 00, opcode 0x31, byte 5 0x00, key count 2",
                "key 1 at byte 7: type 0x01 IRK (Identity Resolving Key), length 16 bytes \
                 [header 01 00 10 00] (data redacted)",
                "key 2 at byte 27: type 0x04 ENC_KEY (Encryption Key), length 16 bytes \
                 [header 04 00 10 00] (data redacted)",
                "the app would take this ENC_KEY: 16 bytes, as it must be",
            ]
        );
        assert_redacted(&lines);
    }

    /// An ENC_KEY of another length is shown, and why the app turns it away.
    #[test]
    fn an_enc_key_of_the_wrong_length_is_turned_away() {
        let lines = key_packet_layout(&key_packet(32));
        assert_eq!(
            lines.last().unwrap(),
            "the app would not take this packet: ENC_KEY must be 16 bytes, got 32"
        );
        assert_redacted(&lines);
    }

    /// A packet cut short says where, and still prints no key byte.
    #[test]
    fn a_truncated_key_packet_says_where_it_ends() {
        let mut packet = key_packet(16);
        packet.truncate(40);
        let lines = key_packet_layout(&packet);
        assert!(
            lines.contains(
                &"key 2 at byte 27: type 0x04 ENC_KEY (Encryption Key), length 16 bytes \
                  declared, 9 left in the packet [header 04 00 10 00] (data redacted)"
                    .to_string()
            ),
            "{lines:#?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "the app would not take this packet: packet too short for key 2 data"
        );
        assert_redacted(&lines);

        let lines = key_packet_layout(&packet[..5]);
        assert_eq!(
            lines[1],
            "preamble cut short: 04 00 04 00 31 (header, opcode, byte 5, key count)"
        );
    }
}
