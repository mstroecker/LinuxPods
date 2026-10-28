//! Checks whether a pair's BLE advertising addresses resolve with the IRK the
//! pair sends over AAP: the hardware check #134 rests on.
//!
//! It asks each pair named on the command line for its keys, as the app does,
//! and keeps them in memory only. Then it scans. Each advertising address is
//! checked once against every pair's IRK, as the IRK comes and byte-reversed,
//! since which order the AirPods send it in is part of the question. Every
//! advertisement is also tried against every pair's ENC_KEY with the app's
//! suffix rule, so the two ways of identifying a pair can be compared.
//!
//! Nothing that identifies a device or a key is printed: the output is meant
//! to be pasted into an issue. Pairs are numbered in the order given, addresses
//! in the order heard. Only the cleartext status bytes 3-8 are shown, which
//! carry no address.
//!
//! A line typed while it runs is printed as a timestamped marker, to tie a
//! phase (lid closed, disconnected) to the output. `q` prints a summary and
//! quits.
//!
//! Quit LinuxPods first, so the probe's AAP session is the only one. Each pair
//! must be connected when the probe starts; it can be disconnected after the
//! keys are in.
//!
//! Usage: cargo run --example irk_probe -- <MAC> [<MAC>...]

use std::collections::HashMap;
use std::io::BufRead;
use std::time::{Duration, Instant};

use aes::Aes128;
use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};
use anyhow::{Context, bail};
use bluer::Address;
use linuxpods::aap::keys::KeyType;
use linuxpods::aap::{Client, Message, decode, find_encryption_key};
use linuxpods::ble::decrypt::decrypt_raw;
use linuxpods::ble::matches_device;
use linuxpods::ble::scanner::{Advertisement, Scanner};
use linuxpods::podstate::Identity;

/// How long to wait for a pair's key packet after the request.
const KEY_WINDOW: Duration = Duration::from_secs(5);

/// The order an IRK resolved an address in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    AsSent,
    Reversed,
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Order::AsSent => "IRK as sent",
            Order::Reversed => "IRK byte-reversed",
        })
    }
}

/// One pair's keys, held in memory only.
struct Pair {
    mac: Address,
    irk: [u8; 16],
    enc_key: [u8; 16],
}

/// The random address hash `ah(k, r)` (Core spec Vol 3 Part H 2.2.2): the low
/// 24 bits of `e(k, 0^104 || r)`. Key, `prand` and result are most significant
/// byte first, as an address is written.
fn ah(irk: &[u8; 16], prand: [u8; 3]) -> [u8; 3] {
    let cipher = Aes128::new_from_slice(irk).expect("an IRK is 16 bytes");
    let mut plain = [0u8; 16];
    plain[13..].copy_from_slice(&prand);
    let mut block = Block::<Aes128>::try_from(&plain[..]).expect("a block is 16 bytes");
    cipher.encrypt_block(&mut block);
    [block[13], block[14], block[15]]
}

/// True for a resolvable private address: the top two bits are `01`.
fn is_rpa(addr: Address) -> bool {
    addr[0] >> 6 == 0b01
}

/// The order in which `irk` resolves `addr`, if it does.
fn resolves(irk: &[u8; 16], addr: Address) -> Option<Order> {
    if !is_rpa(addr) {
        return None;
    }
    let prand = [addr[0], addr[1], addr[2]];
    let hash = [addr[3], addr[4], addr[5]];
    let mut reversed = *irk;
    reversed.reverse();
    if ah(irk, prand) == hash {
        Some(Order::AsSent)
    } else if ah(&reversed, prand) == hash {
        Some(Order::Reversed)
    } else {
        None
    }
}

/// What the address is, without saying which it is.
fn address_kind(addr: Address) -> &'static str {
    match addr[0] >> 6 {
        0b01 => "resolvable private",
        0b00 => "non-resolvable private",
        0b11 => "static random",
        _ => "reserved type",
    }
}

/// Everything learned about one advertising address.
struct Heard {
    /// Its number, in the order addresses were heard.
    n: usize,
    /// The pair whose IRK resolves it, and in which order.
    resolved: Option<(usize, Order)>,
    adverts: u64,
    /// Per pair, how many of its advertisements that pair's ENC_KEY decrypted.
    decrypted: Vec<u64>,
    /// Cleartext bytes 3-8 of the last advertisement: status, battery,
    /// charging, lid, color, connection state.
    status: Vec<u8>,
    first: Duration,
    last: Duration,
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn secs(d: Duration) -> String {
    format!("{:7.1}s", d.as_secs_f64())
}

/// The pairs whose ENC_KEY decrypts this advertisement to a plaintext the app
/// would take.
fn decrypting_pairs(pairs: &[Pair], advert: &Advertisement) -> Vec<usize> {
    let Some(encrypted) = advert.data.encrypted_portion() else {
        return Vec::new();
    };
    pairs
        .iter()
        .enumerate()
        .filter(|(_, pair)| {
            decrypt_raw(encrypted, &pair.enc_key)
                .is_ok_and(|plain| matches_device(&plain, pair.mac))
        })
        .map(|(i, _)| i)
        .collect()
}

fn pair_list(indices: &[usize]) -> String {
    if indices.is_empty() {
        "none".to_string()
    } else {
        indices
            .iter()
            .map(|i| format!("pair {}", i + 1))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Asks a connected pair for its keys over AAP and waits for the reply.
async fn fetch_keys(mac: Address) -> anyhow::Result<Pair> {
    // Dropped at the end, which shuts the socket.
    let client = Client::new(mac).open().await?;
    client.request_proximity_keys().await?;
    let deadline = tokio::time::Instant::now() + KEY_WINDOW;
    let result = loop {
        let packet = match tokio::time::timeout_at(deadline, client.read_packet()).await {
            Ok(read) => read?,
            Err(_) => bail!("no key packet within {}s", KEY_WINDOW.as_secs()),
        };
        if let Ok(Message::Keys(keys)) = decode(&packet) {
            let enc_key = find_encryption_key(&keys)?;
            let irk: [u8; 16] = keys
                .iter()
                .find(|k| k.key_type == KeyType::Irk)
                .context("the key packet carries no IRK")?
                .data
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("the IRK is not 16 bytes"))?;
            break Pair { mac, irk, enc_key };
        }
    };
    Ok(result)
}

fn summary(pairs: &[Pair], heard: &HashMap<Address, Heard>) {
    let mut rows: Vec<&Heard> = heard.values().collect();
    rows.sort_by_key(|h| h.n);
    println!("\n=== summary: {} addresses ===", rows.len());
    for h in rows {
        let resolved = match h.resolved {
            Some((i, order)) => format!("pair {} ({order})", i + 1),
            None => "nothing".to_string(),
        };
        let decrypted = (0..pairs.len())
            .filter(|&i| h.decrypted[i] > 0)
            .map(|i| format!("pair {} {}/{}", i + 1, h.decrypted[i], h.adverts))
            .collect::<Vec<_>>();
        println!(
            "addr {:3}: resolves with {resolved}; decrypts with {}; heard {} to {}",
            h.n,
            if decrypted.is_empty() {
                "none".to_string()
            } else {
                decrypted.join(", ")
            },
            secs(h.first).trim(),
            secs(h.last).trim(),
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=warn".into()),
        )
        .init();

    let macs = std::env::args()
        .skip(1)
        .map(|arg| {
            arg.parse::<Address>()
                .with_context(|| format!("not a MAC address: {arg}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if macs.is_empty() {
        bail!("usage: irk_probe <MAC> [<MAC>...]");
    }

    let mut pairs = Vec::new();
    for (i, mac) in macs.into_iter().enumerate() {
        println!("pair {}: asking for its keys over AAP...", i + 1);
        let pair = fetch_keys(mac)
            .await
            .with_context(|| format!("pair {}", i + 1))?;
        println!("pair {}: IRK and ENC_KEY in (16 bytes each)", i + 1);
        pairs.push(pair);
    }

    let (marks_tx, mut marks) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if marks_tx.send(line).is_err() {
                break;
            }
        }
    });

    let scanner = Scanner::new().await?;
    // Subscribed first, as the first advertisement from an address comes
    // with its device.
    let stream = scanner.advertisements().await?;
    tokio::pin!(stream);
    scanner.start_discovery().await?;
    let start = Instant::now();
    println!("\nscanning - type a line to mark a phase, q to quit\n");

    let mut heard: HashMap<Address, Heard> = HashMap::new();
    // A closed stdin ends the markers, not the scan.
    let mut marking = true;
    loop {
        tokio::select! {
            mark = marks.recv(), if marking => match mark.as_deref() {
                None => marking = false,
                Some("q") => break,
                Some(text) => println!("{} --- {text}", secs(start.elapsed())),
            },
            advert = futures_util::StreamExt::next(&mut stream) => {
                let Some(advert) = advert else { break };
                let now = start.elapsed();
                let decrypting = decrypting_pairs(&pairs, &advert);
                let status = advert.data.raw_data.get(3..9).unwrap_or_default().to_vec();
                let next_n = heard.len() + 1;
                let h = heard.entry(advert.ble_mac).or_insert_with(|| {
                    let resolved = pairs
                        .iter()
                        .enumerate()
                        .find_map(|(i, pair)| resolves(&pair.irk, advert.ble_mac).map(|o| (i, o)));
                    let model = Identity { product_id: advert.data.product_id, color: advert.data.color }.model_name();
                    println!(
                        "{} addr {next_n}: new, {model}, {} address, resolves with {}",
                        secs(now),
                        address_kind(advert.ble_mac),
                        resolved.map_or("nothing".to_string(), |(i, o)| format!("pair {} ({o})", i + 1)),
                    );
                    Heard {
                        n: next_n,
                        resolved,
                        adverts: 0,
                        decrypted: vec![0; pairs.len()],
                        status: Vec::new(),
                        first: now,
                        last: now,
                    }
                });
                h.adverts += 1;
                h.last = now;
                for &i in &decrypting {
                    h.decrypted[i] += 1;
                }
                let agrees = match h.resolved {
                    Some((i, _)) => decrypting == [i],
                    None => decrypting.is_empty(),
                };
                if status != h.status || !agrees {
                    println!(
                        "{} addr {}: bytes 3-8 {} | decrypts with {}{}",
                        secs(now),
                        h.n,
                        hex(&status),
                        pair_list(&decrypting),
                        if agrees { "" } else { "  <-- disagrees with the IRK" },
                    );
                    h.status = status;
                }
            }
        }
    }

    summary(&pairs, &heard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample data of Core spec Vol 3 Part H Appendix D.7.
    const SPEC_IRK: [u8; 16] = [
        0xec, 0x02, 0x34, 0xa3, 0x57, 0xc8, 0xad, 0x05, 0x34, 0x10, 0x10, 0xa6, 0x0a, 0x39, 0x7d,
        0x9b,
    ]; // gitleaks:allow - public sample data, not a device key

    #[test]
    fn ah_matches_the_specification_sample() {
        assert_eq!(ah(&SPEC_IRK, [0x70, 0x81, 0x94]), [0x0d, 0xfb, 0xaa]);
    }

    #[test]
    fn an_address_resolves_in_the_order_its_irk_was_given() {
        let addr = Address::new([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xaa]);
        assert_eq!(resolves(&SPEC_IRK, addr), Some(Order::AsSent));
        let mut reversed = SPEC_IRK;
        reversed.reverse();
        assert_eq!(resolves(&reversed, addr), Some(Order::Reversed));
    }

    #[test]
    fn a_wrong_hash_or_another_address_type_does_not_resolve() {
        let wrong_hash = Address::new([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xab]);
        assert_eq!(resolves(&SPEC_IRK, wrong_hash), None);
        // The same bytes with the top bits 11: a static random address.
        let static_random = Address::new([0xf0, 0x81, 0x94, 0x0d, 0xfb, 0xaa]);
        assert_eq!(resolves(&SPEC_IRK, static_random), None);
    }
}
