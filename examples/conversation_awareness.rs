//! Logs what Conversation Awareness sends: the setting from the startup dump,
//! and every speech level and mode report with its time, for a run of speech
//! and silence against a pair of AirPods.
//!
//! It opens the session the app opens - the features packet included, without
//! which, LibrePods says, the levels come only while nothing plays - and reads
//! what arrives for a while. Speak, stop, speak again, with audio playing or
//! not, and read the times back against `docs/aap-conversation-awareness.md`.
//!
//! With `--on`, a pair whose dump says the setting is off has it turned on for
//! the run, and off again at its end. The packet is LibrePods', not yet tried
//! on a pair from here (#150): whether it took shows as levels while you
//! speak, and in the next connection's dump - run again without `--on`. The
//! setting is persistent, and most likely synced to the user's Apple devices,
//! so it is put back as it was. Interrupted with Ctrl-C, the run cannot put it
//! back: turn it off in the app, on a phone, or with a second run.
//!
//! Nothing is lowered: the volume while the user speaks is the host's to lower,
//! and the app does not do that yet (#150).
//!
//! Usage: cargo run --example conversation_awareness -- <MAC> [SECONDS] [--on]
//!
//! SECONDS is how long to listen, 60 by default.

use std::time::Duration;

use anyhow::Context;
use bluer::Address;
use linuxpods::aap::conversation::{describe_level, set_packet};
use linuxpods::aap::{Client, Message, decode, keys};
use linuxpods::hex;
use tokio::time::Instant;

/// How long to listen, unless the command line says.
const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

/// What a packet is, for the packets this run is about: `None` for the rest.
fn describe(packet: &[u8]) -> Option<String> {
    match decode(packet) {
        Ok(Message::ConversationAwareness(on)) => Some(format!(
            "Conversation Awareness {}",
            if on { "on" } else { "off" }
        )),
        Ok(Message::SpeechLevel(level)) => Some(format!("level {}", describe_level(level))),
        Ok(Message::NoiseMode(mode)) => Some(format!("mode report: {mode}")),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let turn_on = args.iter().any(|a| a == "--on");
    let mut plain = args.iter().filter(|a| *a != "--on");
    let usage = "usage: conversation_awareness <MAC> [SECONDS] [--on]";
    let arg = plain.next().context(usage)?;
    let mac: Address = arg
        .parse()
        .with_context(|| format!("not a MAC address: {arg}"))?;
    let window = match plain.next() {
        Some(seconds) => Duration::from_secs(
            seconds
                .parse()
                .with_context(|| format!("not a number of seconds: {seconds}"))?,
        ),
        None => DEFAULT_WINDOW,
    };

    println!("connecting AAP to {mac}...");
    // The session the app opens, the features included, but not the key
    // request the app makes after it.
    let client = Client::new(mac).open().await?;
    let start = Instant::now();
    println!(
        "connected; listening for {} s - speak, and stop",
        window.as_secs()
    );

    let deadline = start + window;
    let mut turned_on = false;
    let mut others = 0;
    while let Ok(read) = tokio::time::timeout_at(deadline, client.read_packet()).await {
        let packet = match read {
            Ok(packet) => packet,
            Err(e) => {
                println!("read failed: {e:#}");
                break;
            }
        };
        let Some(what) = describe(&packet) else {
            others += 1;
            continue;
        };
        // Redacted, as a key packet would be, though none is asked for:
        // output gets pasted into issues.
        println!(
            "{:7.2} s  {}  {what}",
            start.elapsed().as_secs_f64(),
            keys::redacted_hex(&packet)
        );
        if turn_on
            && !turned_on
            && matches!(decode(&packet), Ok(Message::ConversationAwareness(false)))
        {
            client.set_conversation_awareness(true).await?;
            turned_on = true;
            println!(
                "{:7.2} s  sent {}  on for this run",
                start.elapsed().as_secs_f64(),
                hex(&set_packet(true))
            );
        }
    }
    println!("{others} other packets passed over");

    if turned_on {
        client.set_conversation_awareness(false).await?;
        println!("sent {}  off again, as it was", hex(&set_packet(false)));
        // Room for it to go out before the connection closes.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}
