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

use linuxpods::aap::noise::{NoiseMode, is_noise_mode_packet, parse_noise_mode_packet, set_packet};
use linuxpods::aap::{Client, is_battery_packet, parse_battery_packet};

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

/// Says what a packet is, as far as we can tell. Anything unrecognized is worth
/// seeing in full - that is how the 0x4B notification was found.
fn describe(packet: &[u8]) -> String {
    let cmd = packet.get(4).copied().unwrap_or(0);
    match packet {
        p if is_noise_mode_packet(p) => match parse_noise_mode_packet(p) {
            Ok(mode) => format!("noise control mode report: {mode}"),
            Err(e) => format!("noise control mode report: {e}"),
        },
        p if is_battery_packet(p) => match parse_battery_packet(p) {
            Ok(info) => format!("battery: {info:?}"),
            Err(e) => format!("battery, unparsable: {e}"),
        },
        // The only consistent answer to a mode change, and no use as one: it is
        // byte-identical whichever mode was set.
        _ if cmd == 0x4B => "settings-changed notification (names no mode)".to_string(),
        // Same family as noise control, different sub-command. The startup dump is
        // full of these.
        _ if cmd == 0x09 => format!(
            "settings packet, sub-command 0x{:02X}",
            packet.get(6).copied().unwrap_or(0)
        ),
        _ => format!("unknown, command byte 0x{cmd:02X}"),
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
                    hex(&packet),
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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=debug".into()),
        )
        .init();

    let mac = std::env::args().nth(1).expect("usage: noise_probe <MAC>");

    let mut client = Client::new(&mac)?;
    println!("connecting AAP to {mac}...");
    client.connect().await?;
    client.handshake().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.request_battery_status().await?;
    // Without this, Adaptive is unavailable.
    client.enable_special_features().await?;
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
