//! Logs what Conversation Awareness sends: the setting from the startup dump,
//! and every speech level and mode report with its time, for a run of speech
//! and silence against a pair of AirPods - and with `--duck`, lowers the
//! pair's volume while the user speaks, as the app does.
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
//! With `--duck`, the levels lower the pair's own A2DP volume - BlueZ's
//! `MediaTransport1.Volume`, on the system bus - as the app lowers it, with
//! the same logic (`linuxpods::volume::Ducking`): to a fifth at the first
//! `01`/`02`, faded back over 1.5 s from the first level of `03` or above,
//! only while A2DP plays, and left alone once you move it yourself. Each
//! volume set is logged with its time, and with what the transport read back
//! after it and how long that took, as BlueZ changes `Volume` only once the
//! pods answer, with the volume they report
//! (`docs/aap-conversation-awareness.md`); waiting for that, the example
//! takes the next level later than the app would. What was lowered is put
//! back at the end of the run; interrupted with Ctrl-C, it is not. Quit the app
//! first, or both lower the volume.
//!
//! Usage: cargo run --example conversation_awareness -- <MAC> [SECONDS] [--on] [--duck]
//!
//! SECONDS is how long to listen, 60 by default.

use std::time::Duration;

use anyhow::Context;
use bluer::Address;
use linuxpods::aap::conversation::{describe_level, set_packet};
use linuxpods::aap::{Client, Message, decode, keys};
use linuxpods::hex;
use linuxpods::volume::{A2dp, Ducking, find_a2dp, set_volume};
use tokio::time::Instant;
use zbus::Connection;

/// How long the transport is read after a set, for the pods' answer.
const READ_BACK_LIMIT: Duration = Duration::from_secs(2);

/// The volume the pair's A2DP transport reads once it has moved from `was`,
/// after a set: what the pods answered. `None` if it did not move within
/// [`READ_BACK_LIMIT`], or the transport went.
async fn read_back(conn: &Connection, mac: Address, was: u16) -> anyhow::Result<Option<u16>> {
    let deadline = Instant::now() + READ_BACK_LIMIT;
    while Instant::now() < deadline {
        let Some(a2dp) = find_a2dp(conn, mac).await? else {
            return Ok(None);
        };
        if a2dp.transport.volume != was {
            return Ok(Some(a2dp.transport.volume));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(None)
}

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

/// The pair's volume while the user speaks, with `--duck`: the app's logic,
/// on the system bus.
struct Duck {
    conn: Connection,
    mac: Address,
    ducking: Ducking,
    start: Instant,
}

impl Duck {
    /// Takes a speech level.
    async fn level(&mut self, level: u8) -> anyhow::Result<()> {
        let found = find_a2dp(&self.conn, self.mac).await?;
        let set = self.ducking.level(
            level,
            found.as_ref().map(|a| a.transport),
            std::time::Instant::now(),
        );
        self.apply(found, set, "lowered").await
    }

    /// Takes a fade's step, or a look for a transport that went, if one is
    /// due. Unread, the transport counts as gone, which waits for it rather
    /// than trying again at once.
    async fn step(&mut self) -> anyhow::Result<()> {
        let found = match find_a2dp(&self.conn, self.mac).await {
            Ok(found) => found,
            Err(e) => {
                self.ducking.step(None, std::time::Instant::now());
                return Err(e);
            }
        };
        let set = self.ducking.step(
            found.as_ref().map(|a| a.transport),
            std::time::Instant::now(),
        );
        self.apply(found, set, "faded").await
    }

    /// Puts back what was lowered.
    async fn restore(&mut self) -> anyhow::Result<()> {
        let found = find_a2dp(&self.conn, self.mac).await?;
        let set = self.ducking.restore(found.as_ref().map(|a| a.transport));
        self.apply(found, set, "put back").await
    }

    /// Sets the volume, and reads the transport until it moves, to log
    /// what the pods answered and how long that took - the hardware check of
    /// how they answer. The app does not wait for it.
    async fn apply(
        &mut self,
        found: Option<A2dp>,
        set: Option<u16>,
        what: &str,
    ) -> anyhow::Result<()> {
        let (Some(a2dp), Some(volume)) = (found, set) else {
            return Ok(());
        };
        set_volume(&self.conn, &a2dp.path, volume).await?;
        let was = a2dp.transport.volume;
        println!(
            "{:7.2} s  volume {what}: {was} -> {volume} of 127 at {}",
            self.start.elapsed().as_secs_f64(),
            a2dp.path
        );
        if volume == was {
            return Ok(());
        }
        let sent = Instant::now();
        match read_back(&self.conn, self.mac, was).await? {
            Some(answered) => {
                println!(
                    "{:7.2} s  read back {answered} after {} ms",
                    self.start.elapsed().as_secs_f64(),
                    sent.elapsed().as_millis()
                );
            }
            None => println!(
                "{:7.2} s  still {was} {} ms after the set",
                self.start.elapsed().as_secs_f64(),
                sent.elapsed().as_millis()
            ),
        }
        Ok(())
    }

    /// When the next fade step is due, as the runtime's clock has it.
    fn next_due(&self) -> Option<Instant> {
        self.ducking.next_due().map(Instant::from_std)
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
    let flags = ["--on", "--duck"];
    let turn_on = args.iter().any(|a| a == "--on");
    let duck = args.iter().any(|a| a == "--duck");
    let mut plain = args.iter().filter(|a| !flags.contains(&a.as_str()));
    let usage = "usage: conversation_awareness <MAC> [SECONDS] [--on] [--duck]";
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

    let mut duck = match duck {
        true => Some(Duck {
            conn: Connection::system()
                .await
                .context("failed to connect to the system bus")?,
            mac,
            ducking: Ducking::default(),
            start: Instant::now(),
        }),
        false => None,
    };

    println!("connecting AAP to {mac}...");
    // The session the app opens, the features included, but not the key
    // request the app makes after it.
    let client = Client::new(mac).open().await?;
    let start = Instant::now();
    if let Some(duck) = &mut duck {
        duck.start = start;
        match find_a2dp(&duck.conn, mac).await? {
            Some(a2dp) => println!(
                "A2DP transport {}: volume {} of 127, {}",
                a2dp.path,
                a2dp.transport.volume,
                if a2dp.transport.playing {
                    "playing"
                } else {
                    "not playing, or in a call"
                }
            ),
            None => println!("no A2DP transport: nothing plays to {mac}, nothing to lower"),
        }
    }
    println!(
        "connected; listening for {} s - speak, and stop",
        window.as_secs()
    );

    let deadline = start + window;
    let mut turned_on = false;
    let mut others = 0;
    loop {
        let wake = duck
            .as_ref()
            .and_then(Duck::next_due)
            .map_or(deadline, |step| step.min(deadline));
        let packet = match tokio::time::timeout_at(wake, client.read_packet()).await {
            Ok(Ok(packet)) => packet,
            Ok(Err(e)) => {
                println!("read failed: {e:#}");
                break;
            }
            Err(_) if wake >= deadline => break,
            Err(_) => {
                if let Some(duck) = &mut duck
                    && let Err(e) = duck.step().await
                {
                    println!("fade ended: {e:#}");
                }
                continue;
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
        match decode(&packet) {
            Ok(Message::SpeechLevel(level)) => {
                if let Some(duck) = &mut duck
                    && let Err(e) = duck.level(level).await
                {
                    println!("level not acted on: {e:#}");
                }
            }
            Ok(Message::ConversationAwareness(false)) if turn_on && !turned_on => {
                client.set_conversation_awareness(true).await?;
                turned_on = true;
                println!(
                    "{:7.2} s  sent {}  on for this run",
                    start.elapsed().as_secs_f64(),
                    hex(&set_packet(true))
                );
            }
            _ => {}
        }
    }
    println!("{others} other packets passed over");

    // Logged, not returned: the setting is put back below either way.
    if let Some(duck) = &mut duck
        && let Err(e) = duck.restore().await
    {
        println!("volume not put back: {e:#}");
    }
    if turned_on {
        client.set_conversation_awareness(false).await?;
        println!("sent {}  off again, as it was", hex(&set_packet(false)));
        // Room for it to go out before the connection closes.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}
