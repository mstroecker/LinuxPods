//! Cycles the noise control modes and classifies everything that comes back.
//!
//! This is the tool the packet format in `docs/aap-noise-control.md` was verified
//! with, ported from the Go original. It talks to the client directly rather than
//! through the coordinator, so the device's answers can be read unfiltered - which
//! is the whole point: what arrives after a mode change is a generic
//! settings-changed notification, not a usable acknowledgement.
//!
//! Every mode should be audible on the AirPods. Listen while it runs.
//!
//! Usage: cargo run --example noise_probe -- <MAC>

use std::time::Duration;

use anyhow::Context;
use bluer::Address;
use linuxpods::aap::noise::{NoiseMode, set_packet};
use linuxpods::aap::{Client, Message, decode, keys};

/// How long to listen after each command. The 0x0D echo, when it comes at all,
/// took seconds to arrive on the hardware this was verified against.
const RESPONSE_WINDOW: Duration = Duration::from_secs(4);

/// Packets from the previous mode change still in flight are drained for this
/// long, so they are not mistaken for an answer to the next one.
const DRAIN_WINDOW: Duration = Duration::from_secs(3);

fn hex(packet: &[u8]) -> String {
    packet
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// A received packet as it is printed: whole, except a key packet, of which
/// only the header, opcode and key count go out - see [`keys::redacted_hex`].
/// Probe output gets pasted into issues. The probe asks for no keys, but a key
/// packet is held back whether asked for or not, parsable or not.
fn shown(packet: &[u8]) -> String {
    keys::redacted_hex(packet)
}

/// Says what a packet is, as far as we can tell. Anything unrecognized is worth
/// seeing in full - that is how the 0x4B notification was found.
fn describe(packet: &[u8]) -> String {
    match decode(packet) {
        Ok(Message::NoiseMode(mode)) => format!("noise control mode report: {mode}"),
        Ok(Message::Battery(info)) => format!("battery: {info:?}"),
        Ok(Message::Keys(keys)) => format!("proximity keys: {} of them", keys.len()),
        // The only consistent answer to a mode change, and no use as one: it is
        // byte-identical whichever mode was set.
        Ok(Message::Unknown { opcode: Some(0x4B) }) => {
            "settings-changed notification (names no mode)".to_string()
        }
        // Same family as noise control, different sub-command. The startup dump is
        // full of these.
        Ok(Message::Unknown { opcode: Some(0x09) }) => format!(
            "settings packet, sub-command 0x{:02X}",
            packet.get(6).copied().unwrap_or(0)
        ),
        Ok(Message::Unknown { opcode: Some(cmd) }) => format!("unknown, command byte 0x{cmd:02X}"),
        Ok(Message::Unknown { opcode: None }) => "unknown, no AAP header".to_string(),
        Err(e) => format!("unparsable: {e}"),
    }
}

/// Reads packets until `window` elapses, printing what each one is.
async fn listen(client: &Client, window: Duration, prefix: &str) -> usize {
    let deadline = tokio::time::Instant::now() + window;
    let mut count = 0;

    while let Ok(read) = tokio::time::timeout_at(deadline, client.read_packet()).await {
        match read {
            Ok(packet) => {
                count += 1;
                println!(
                    "  {prefix} {} ({} bytes) - {}",
                    shown(&packet),
                    packet.len(),
                    describe(&packet)
                );
            }
            Err(e) => {
                println!("  {prefix} read failed: {e:#}");
                break;
            }
        }
    }
    count
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=debug".into()),
        )
        .init();

    let arg = std::env::args().nth(1).expect("usage: noise_probe <MAC>");
    let mac: Address = arg
        .parse()
        .with_context(|| format!("not a MAC address: {arg}"))?;

    println!("connecting AAP to {mac}...");
    // The session the app opens, the features included - without them,
    // Adaptive is unavailable - but not the key request the app makes after.
    let client = Client::new(mac).open().await?;
    println!("connected");

    // The startup dump reports the mode the device is actually in - the one
    // reliable report in the whole exchange.
    println!("\ndraining the startup dump:");
    listen(&client, DRAIN_WINDOW, "dump").await;

    for (i, mode) in NoiseMode::ALL.into_iter().enumerate() {
        println!("\n--- {} ---", mode.label());
        println!("  sent {}", hex(&set_packet(mode)));
        client.set_noise_mode(mode).await?;

        let received = listen(&client, RESPONSE_WINDOW, "recv").await;
        if received == 0 {
            println!("  nothing came back");
        }

        if i + 1 < NoiseMode::ALL.len() {
            listen(&client, DRAIN_WINDOW, "drain").await;
        }
    }

    // Off is the one mode recent firmware gates behind a setting of its own: the
    // bare command above should have produced an error chime and no change. Ask
    // again with that setting enabled, back to back and with no delay, exactly
    // as the coordinator does it.
    println!("\n--- Off, with allow-off enabled ---");
    client.set_allow_off_listening_mode(true).await?;
    println!(
        "  sent {}",
        hex(&linuxpods::aap::noise::allow_off_packet(true))
    );
    client.set_noise_mode(NoiseMode::Off).await?;
    println!("  sent {}", hex(&set_packet(NoiseMode::Off)));
    listen(&client, RESPONSE_WINDOW, "recv").await;

    println!("\ndone - the modes should each have been audible");
    println!("Off: chime then no change on the first attempt, silence and a real");
    println!("switch on the second, is the 0x34 setting doing its job.");
    Ok(())
}
