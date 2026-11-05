//! Reads the name the pods give every host, and with a new name given, renames
//! them over AAP and shows what comes back.
//!
//! The name is the one in the device information packet (0x1D) the pods send
//! on every connection, and the one BlueZ shows as `Device1.Name` - not the
//! alias BlueZ keeps for this machine alone. The probe reads BlueZ's `Name`
//! and `Alias`, and changes nothing there.
//!
//! The rename packet is LibrePods', confirmed on a Pro 3 with this probe on
//! 2026-11-02 (#160, `docs/protocol/aap/rename.md`). Whether it took, and what it takes
//! for it to show, is read in three places, each a step further from the
//! rename:
//!
//! 1. whatever the pods send in the window after it, a device information
//!    packet of their own accord included
//! 2. the device information of a second AAP session, opened once the first
//!    is closed - the Bluetooth link stays up, held by audio
//! 3. BlueZ's `Device1.Name`, before the rename and after the second session:
//!    bluetoothd reads the remote name when the link comes up, so it changes
//!    only once the pair has disconnected and connected again
//!
//! If BlueZ still shows the old name, disconnect the pair, connect it again,
//! and run the probe without a name.
//!
//! The name is persistent on the pods: rename them back with a second run.
//! Apple devices keep the name they have: an iPhone did not follow the run's
//! rename.
//!
//! Quit LinuxPods first, so the probe's AAP session is the only one.
//!
//! Of the device information packet only the name is printed: the rest holds
//! the model number and serial numbers, and probe output gets pasted into
//! issues. A name the app would refuse - empty, with a NUL, over 127 bytes -
//! is refused here too, before connecting.
//!
//! With `--sweep`, it tries a list of names instead - characters beyond ASCII,
//! and lengths from 32 bytes to the length byte's 255 - and for each one
//! renames, opens a second session, and compares the name the pods report byte
//! for byte with the name sent (#160). The names past the 127 bytes the pods
//! keep go out unchecked, as the app never sends them. It renames the pods
//! back to the name they had at the start, also when a name ends the sweep
//! early.
//!
//! Usage: cargo run --example rename_probe -- <MAC> [NEW NAME]
//!        cargo run --example rename_probe -- <MAC> --sweep

use std::time::Duration;

use anyhow::Context;
use bluer::Address;
use linuxpods::aap::info::{info_name_bytes, is_info_packet, parse_device_info};
use linuxpods::aap::name::rename_packet;
use linuxpods::aap::{Client, keys, message};
use linuxpods::hex;
use tokio::time::Instant;

/// How long the startup dump is read for, the device information in it.
const DUMP_WINDOW: Duration = Duration::from_secs(5);

/// How long to listen after the rename.
const RESPONSE_WINDOW: Duration = Duration::from_secs(15);

/// How long the first session has to close before the second opens.
const REOPEN_PAUSE: Duration = Duration::from_secs(2);

/// How long the sweep listens after each rename, for anything the pods answer.
const SWEEP_LISTEN: Duration = Duration::from_secs(3);

/// The names the sweep tries, with what each is for: the characters first,
/// then the lengths, the longest last, since a name that drops the link ends
/// the sweep.
fn sweep_names() -> Vec<(&'static str, String)> {
    // An ASCII name of exactly `len` bytes that says its own length.
    let ascii = |len: usize| {
        let mut name = format!("Len {len} ");
        while name.len() < len {
            name.push(char::from(b'a' + (name.len() % 26) as u8));
        }
        name
    };
    vec![
        ("umlauts, 2 bytes each", "Müllers Pöds".to_string()),
        ("CJK, 3 bytes each", "耳机测试".to_string()),
        ("emoji outside the BMP, 4 bytes", "Pods 🎧".to_string()),
        ("ZWJ emoji sequence", "Pods 👩\u{200d}💻".to_string()),
        ("combining accent", "Cafe\u{301} Pods".to_string()),
        ("right-to-left script", "سماعات".to_string()),
        ("ASCII, 32 bytes", ascii(32)),
        ("ASCII, 33 bytes", ascii(33)),
        ("17 umlauts, 34 bytes", "ü".repeat(17)),
        ("ASCII, 64 bytes", ascii(64)),
        ("ASCII, 128 bytes", ascii(128)),
        ("ASCII, 248 bytes", ascii(248)),
        ("ASCII, 255 bytes", ascii(255)),
    ]
}

/// Opens a session and reads the name from its startup dump.
async fn open_and_read(mac: Address) -> anyhow::Result<(Client, Vec<u8>)> {
    let client = Client::new(mac).open().await?;
    let deadline = Instant::now() + DUMP_WINDOW;
    loop {
        let packet = tokio::time::timeout_at(deadline, client.read_packet())
            .await
            .context("no device information in the startup dump")??;
        if let Some(name) = info_name_bytes(&packet) {
            return Ok((client, name.to_vec()));
        }
    }
}

/// Bytes as text where they are UTF-8, and as hex where they are not.
fn text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => format!("{s:?}"),
        Err(_) => format!("not UTF-8: {}", hex(bytes)),
    }
}

/// Renames to each of [`sweep_names`] and reads the name back from a second
/// session, then renames back to the name the pods had.
async fn sweep(mac: Address) -> anyhow::Result<()> {
    let start = Instant::now();
    println!("connecting AAP to {mac}...");
    let (first, original) = open_and_read(mac).await?;
    let original_name = String::from_utf8(original.clone())
        .context("the pods' own name is not UTF-8, and could not be put back")?;
    println!(
        "name at the start: {} ({} bytes)\n",
        text(&original),
        original.len()
    );

    let mut client = Some(first);
    let mut results = Vec::new();
    for (what, name) in sweep_names() {
        let Some(session) = client.take() else { break };
        println!("--- {what}: {name:?} ({} bytes)", name.len());
        // Unchecked, to reach past what the app sends.
        if let Err(e) = session.rename_unchecked(name.as_bytes()).await {
            println!("  send failed: {e:#}");
            results.push((what, "send failed".to_string()));
            break;
        }
        listen(&session, SWEEP_LISTEN, start, true).await;
        drop(session);
        tokio::time::sleep(REOPEN_PAUSE).await;
        match open_and_read(mac).await {
            Ok((next, got)) => {
                let verdict = if got == name.as_bytes() {
                    "kept".to_string()
                } else {
                    format!("changed: {} ({} bytes)", text(&got), got.len())
                };
                println!("  {verdict}");
                results.push((what, verdict));
                client = Some(next);
            }
            Err(e) => {
                println!("  no second session: {e:#}");
                results.push((what, format!("no second session: {e:#}")));
            }
        }
    }

    println!("\n--- back to {original_name:?}");
    let session = match client {
        Some(session) => session,
        None => open_and_read(mac).await?.0,
    };
    session.rename(&original_name).await?;
    drop(session);
    tokio::time::sleep(REOPEN_PAUSE).await;
    let (_, got) = open_and_read(mac).await?;
    println!(
        "  {}",
        if got == original {
            "back".to_string()
        } else {
            format!("still {}", text(&got))
        }
    );

    println!("\nsummary:");
    for (what, verdict) in results {
        println!("  {what:32} {verdict}");
    }
    Ok(())
}

/// BlueZ's `Name` and `Alias` for the device with address `mac`, on whichever
/// adapter it is. Read only: nothing is set.
async fn bluez_names(mac: Address) -> anyhow::Result<(String, String)> {
    let conn = zbus::Connection::system().await?;
    let manager = zbus::fdo::ObjectManagerProxy::builder(&conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let objects = manager.get_managed_objects().await?;
    let want = mac.to_string();
    for interfaces in objects.values() {
        let Some(device) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        let text = |key: &str| {
            device
                .get(key)
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        };
        if text("Address").as_deref() == Some(want.as_str()) {
            return Ok((
                text("Name").unwrap_or_default(),
                text("Alias").unwrap_or_default(),
            ));
        }
    }
    anyhow::bail!("BlueZ knows no device {mac}")
}

async fn print_bluez_names(mac: Address, when: &str) {
    match bluez_names(mac).await {
        Ok((name, alias)) => println!("BlueZ {when}: Name {name:?}, Alias {alias:?}"),
        Err(e) => println!("BlueZ {when}: not read: {e:#}"),
    }
}

/// A packet as it is printed: a device information packet as its name only -
/// a 0x1D shaped otherwise as its preamble only, as a key packet - anything
/// else whole.
fn shown(packet: &[u8]) -> String {
    if let Ok(info) = parse_device_info(packet) {
        return match info.name {
            Ok(name) => format!("device information, name {name:?}"),
            Err(e) => format!("device information, no name read: {e}"),
        };
    }
    let opcode = message::opcode(packet)
        .map(|op| format!("opcode 0x{op:02X}"))
        .unwrap_or_else(|| "no AAP header".to_string());
    format!("{opcode}: {}", keys::redacted_hex(packet))
}

/// Reads packets until `window` elapses. With `all`, prints every one;
/// otherwise only device information, and how many others came.
async fn listen(client: &Client, window: Duration, start: Instant, all: bool) {
    let deadline = Instant::now() + window;
    let mut others = 0;
    while let Ok(read) = tokio::time::timeout_at(deadline, client.read_packet()).await {
        match read {
            Ok(packet) => {
                if all || is_info_packet(&packet) {
                    println!(
                        "  +{:6.2}s {} ({} bytes)",
                        start.elapsed().as_secs_f64(),
                        shown(&packet),
                        packet.len()
                    );
                } else {
                    others += 1;
                }
            }
            Err(e) => {
                println!("  read failed: {e:#}");
                break;
            }
        }
    }
    if !all {
        println!("  and {others} other packets");
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

    let mut args = std::env::args().skip(1);
    let arg = args
        .next()
        .expect("usage: rename_probe <MAC> [NEW NAME | --sweep]");
    let mac: Address = arg
        .parse()
        .with_context(|| format!("not a MAC address: {arg}"))?;
    let rest: Vec<String> = args.collect();
    if rest == ["--sweep"] {
        return sweep(mac).await;
    }
    let new_name = (!rest.is_empty()).then(|| rest.join(" "));
    // Refused before connecting, not after.
    let packet = new_name.as_deref().map(rename_packet).transpose()?;

    print_bluez_names(mac, "before").await;
    println!("\nconnecting AAP to {mac}...");
    let client = Client::new(mac).open().await?;
    let start = Instant::now();
    println!("connected\n\nstartup dump:");
    listen(&client, DUMP_WINDOW, start, false).await;

    let (Some(name), Some(packet)) = (new_name, packet) else {
        return Ok(());
    };

    println!("\nrenaming to {name:?}");
    client.rename(&name).await?;
    println!(
        "  +{:6.2}s sent {}",
        start.elapsed().as_secs_f64(),
        hex(&packet)
    );
    listen(&client, RESPONSE_WINDOW, start, true).await;

    println!("\nclosing the session, and opening a second one");
    drop(client);
    tokio::time::sleep(REOPEN_PAUSE).await;
    let client = Client::new(mac).open().await?;
    println!("  +{:6.2}s connected again", start.elapsed().as_secs_f64());
    listen(&client, DUMP_WINDOW, start, false).await;
    drop(client);

    println!();
    print_bluez_names(mac, "after").await;
    println!("\nif BlueZ still shows the old name, disconnect and connect the pair,");
    println!("then run again without a name");
    Ok(())
}
