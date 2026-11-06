//! Listens for the pods through a BlueZ advertisement monitor instead of
//! discovery, to answer the open questions of #189 before the scanner changes:
//! why discovery blocks some devices, and what a monitor is, in
//! [protocol/linux/bluez.md](../docs/protocol/linux/bluez.md#discovery-and-directed-advertising).
//!
//! It registers one `or_patterns` monitor on every adapter, matching
//! manufacturer data that starts `4C 00 07` (Apple, proximity pairing), with
//! `RSSISamplingPeriod` 0 and no RSSI threshold, and starts no discovery
//! itself.
//!
//! It prints, with seconds since the start:
//!
//! - each adapter's `Discovering` at the start and every change of it: a run
//!   says something about passive scanning only while it reads false;
//! - the monitor's `Activate` and `Release`, and each `DeviceFound`, with the
//!   device's proximity payload as BlueZ holds it then, and `DeviceLost`;
//! - each proximity pairing advertisement BlueZ signals: in the
//!   `InterfacesAdded` that announces a device, or a `ManufacturerData`
//!   change, with the model, the status byte and the payload's length;
//! - the removal (`InterfacesRemoved`) of each device heard so: a payload or a
//!   `DeviceFound`;
//! - every device's `Connected` change, and every device announced connected,
//!   whatever the device: to see whether a device that reconnects by directed
//!   advertising, such as #189's mouse, connects while the monitor is
//!   registered.
//!
//! `RSSI` changes are counted, not printed. A line typed while it runs is
//! printed as a timestamped marker, to tie a phase (lid open, mouse switched
//! on) to the output. `q` or ctrl-c unregisters the monitor and prints a
//! summary per address: when first and last heard, payloads, `RSSI` signals
//! and their rate, removals, and `DeviceFound` and `DeviceLost`.
//!
//! `--discover` also starts discovery on this connection with
//! `Scanner::start_discovery`, which sets the app's discovery filter, with the
//! monitor still registered: the same output with the active scan on, to
//! compare the two.
//!
//! Quit LinuxPods first, and close GNOME Settings' Bluetooth panel: both
//! discover. Decrypts nothing and prints no payload bytes. Prints the random
//! address of every device in range that advertises proximity pairing data,
//! other people's AirPods among them, or that the monitor reports found or
//! lost, and the address of every device whose `Connected` changes or that is
//! announced connected.
//!
//! Usage: cargo run --example adv_monitor_probe [-- --discover]

use std::collections::HashMap;
use std::io::BufRead;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use linuxpods::ble::parser::parse_proximity_data;
use linuxpods::bluez::{self, BLUEZ_SERVICE};
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream, interface, proxy};

const ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";
const DEVICE_INTERFACE: &str = "org.bluez.Device1";
const APPLE_COMPANY_ID: u16 = 0x004C;
/// Manufacturer specific data, in the Core spec's assigned numbers.
const AD_MANUFACTURER_DATA: u8 = 0xFF;
/// Apple's company ID, little-endian, then the proximity pairing type
/// ([protocol/ble/proximity-pairing.md](../docs/protocol/ble/proximity-pairing.md)).
const PROXIMITY_PATTERN: [u8; 3] = [0x4C, 0x00, 0x07];

#[proxy(
    interface = "org.bluez.AdvertisementMonitorManager1",
    default_service = "org.bluez"
)]
trait AdvertisementMonitorManager1 {
    fn register_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property)]
    fn supported_features(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn supported_monitor_types(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
}

/// What BlueZ tells the monitor.
enum MonitorEvent {
    Activated,
    Released,
    Found(OwnedObjectPath),
    Lost(OwnedObjectPath),
}

/// The monitor BlueZ reads and calls (`org.bluez.AdvertisementMonitor1`,
/// BlueZ 5.87's `doc/org.bluez.AdvertisementMonitor.rst`). No RSSI threshold
/// is set, so none filters.
struct Monitor {
    events: mpsc::UnboundedSender<MonitorEvent>,
}

#[interface(name = "org.bluez.AdvertisementMonitor1")]
impl Monitor {
    fn release(&self) {
        let _ = self.events.send(MonitorEvent::Released);
    }

    fn activate(&self) {
        let _ = self.events.send(MonitorEvent::Activated);
    }

    fn device_found(&self, device: OwnedObjectPath) {
        let _ = self.events.send(MonitorEvent::Found(device));
    }

    fn device_lost(&self, device: OwnedObjectPath) {
        let _ = self.events.send(MonitorEvent::Lost(device));
    }

    #[zbus(property, name = "Type")]
    fn monitor_type(&self) -> String {
        "or_patterns".into()
    }

    /// 0, and what it asks of bluetoothd:
    /// [protocol/linux/bluez.md](../docs/protocol/linux/bluez.md#discovery-and-directed-advertising).
    #[zbus(property, name = "RSSISamplingPeriod")]
    fn rssi_sampling_period(&self) -> u16 {
        0
    }

    /// Start position, AD type, content.
    #[zbus(property)]
    fn patterns(&self) -> Vec<(u8, u8, Vec<u8>)> {
        vec![(0, AD_MANUFACTURER_DATA, PROXIMITY_PATTERN.to_vec())]
    }
}

/// What was heard from one address.
#[derive(Default)]
struct Heard {
    first: Duration,
    last: Duration,
    payloads: u32,
    rssi: u32,
    removed: u32,
    found: u32,
    lost: u32,
}

/// What one BlueZ signal says, as far as this probe reads it.
enum Signal {
    /// A device announced, with the properties it came with.
    Added(String, HashMap<String, OwnedValue>),
    Removed(String),
    DeviceChanged(String, HashMap<String, OwnedValue>),
    Discovering(String, bool),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let discover = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--discover") => true,
        Some(other) => bail!("unknown argument {other}; usage: adv_monitor_probe [--discover]"),
    };
    let root = format!("{}/probe_monitor", linuxpods::APP_PATH);
    let root_path = ObjectPath::try_from(root.as_str())?;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    // Both objects served at build, before `RegisterMonitor`:
    // [app/bluez.md](../docs/app/bluez.md#connections-that-serve-objects).
    let conn = zbus::connection::Builder::system()?
        .serve_at(root.as_str(), zbus::fdo::ObjectManager)?
        .serve_at(format!("{root}/m0"), Monitor { events: events_tx })?
        .build()
        .await
        .context("failed to connect to the system bus")?;

    let start = Instant::now();
    let mut signals = signals(&conn).await?;

    let adapters = bluez::adapters(&conn).await?;
    if adapters.is_empty() {
        bail!("BlueZ has no adapter");
    }
    let mut registered = Vec::new();
    for adapter in &adapters {
        let manager = AdvertisementMonitorManager1Proxy::builder(&conn)
            .path(adapter.path.as_str())?
            .build()
            .await?;
        let discovering = Adapter1Proxy::builder(&conn)
            .path(adapter.path.as_str())?
            .build()
            .await?
            .discovering()
            .await
            .unwrap_or(false);
        println!(
            "{} {}: powered={}, discovering={discovering}, monitor types={:?}, features={:?}",
            secs(start.elapsed()),
            adapter.path,
            adapter.powered,
            manager.supported_monitor_types().await.unwrap_or_default(),
            manager.supported_features().await.unwrap_or_default(),
        );
        match manager.register_monitor(&root_path).await {
            Ok(()) => {
                println!(
                    "{} {}: monitor registered",
                    secs(start.elapsed()),
                    adapter.path
                );
                registered.push(manager);
            }
            Err(e) => println!(
                "{} {}: register failed: {e}",
                secs(start.elapsed()),
                adapter.path
            ),
        }
    }
    if registered.is_empty() {
        bail!("no adapter took the monitor");
    }
    if discover {
        // After the subscription in `signals`, so the first advertisement
        // from each address, in its InterfacesAdded, is heard.
        let scanning = linuxpods::ble::scanner::Scanner::with_connection(conn.clone())
            .start_discovery()
            .await?;
        println!(
            "{} discovery started on {scanning:?}",
            secs(start.elapsed())
        );
    }

    let (marks_tx, mut marks) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if marks_tx.send(line).is_err() {
                break;
            }
        }
    });
    println!("\nlistening - type a line to mark a phase, q or ctrl-c to quit\n");

    let mut heard: HashMap<String, Heard> = HashMap::new();
    // A closed stdin ends the markers, not the run.
    let mut marking = true;
    loop {
        tokio::select! {
            mark = marks.recv(), if marking => match mark.as_deref() {
                None => marking = false,
                Some("q") => break,
                Some(text) => println!("{} --- {text}", secs(start.elapsed())),
            },
            _ = tokio::signal::ctrl_c() => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                let now = start.elapsed();
                match event {
                    MonitorEvent::Activated => println!("{} monitor activated", secs(now)),
                    MonitorEvent::Released => println!("{} monitor released", secs(now)),
                    MonitorEvent::Found(path) => {
                        let addr = address(path.as_str());
                        let line = match device_properties(&conn, &path).await {
                            Ok(props) => proximity(&props).unwrap_or_else(|| "no proximity payload".into()),
                            Err(e) => format!("properties not read: {e}"),
                        };
                        println!("{} {addr}: DeviceFound, {line}", secs(now));
                        entry(&mut heard, &addr, now).found += 1;
                    }
                    MonitorEvent::Lost(path) => {
                        let addr = address(path.as_str());
                        println!("{} {addr}: DeviceLost", secs(now));
                        entry(&mut heard, &addr, now).lost += 1;
                    }
                }
            }
            signal = signals.recv() => {
                let Some((at, signal)) = signal else { break };
                handle(signal, at.duration_since(start), &mut heard);
            }
        }
    }

    for manager in &registered {
        if let Err(e) = manager.unregister_monitor(&root_path).await {
            println!("{}: unregister failed: {e}", manager.inner().path());
        }
    }
    summary(&heard, start.elapsed());
    Ok(())
}

/// The device's properties as BlueZ holds them now: a device BlueZ kept
/// from before is not announced again.
async fn device_properties(
    conn: &Connection,
    path: &OwnedObjectPath,
) -> Result<HashMap<String, OwnedValue>> {
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(BLUEZ_SERVICE)?
        .path(path.as_str())?
        .build()
        .await?;
    let iface = zbus::names::InterfaceName::try_from(DEVICE_INTERFACE)?;
    Ok(props.get_all(iface).await?)
}

/// Every signal bluetoothd sends, in one stream so their order holds, each with
/// the time it was read. A task of its own drains the subscription as the
/// signals arrive, so the calls to BlueZ in `main` never wait on an unread
/// queue: [app/bluez.md](../docs/app/bluez.md#subscriptions-that-advertisements-reach).
async fn signals(conn: &Connection) -> Result<mpsc::UnboundedReceiver<(Instant, Signal)>> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BLUEZ_SERVICE)?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, conn, None)
        .await
        .context("failed to subscribe to BlueZ's signals")?;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else { continue };
            let Some(signal) = read_signal(&msg) else {
                continue;
            };
            if tx.send((Instant::now(), signal)).is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

fn read_signal(msg: &Message) -> Option<Signal> {
    let header = msg.header();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
            let (path, mut interfaces) = msg
                .body()
                .deserialize::<(
                    OwnedObjectPath,
                    HashMap<String, HashMap<String, OwnedValue>>,
                )>()
                .ok()?;
            let props = interfaces.remove(DEVICE_INTERFACE)?;
            Some(Signal::Added(address(path.as_str()), props))
        }
        ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
            let (path, interfaces) = msg
                .body()
                .deserialize::<(OwnedObjectPath, Vec<String>)>()
                .ok()?;
            interfaces
                .iter()
                .any(|i| i == DEVICE_INTERFACE)
                .then(|| Signal::Removed(address(path.as_str())))
        }
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path = header.path()?.to_string();
            let (iface, changed, _invalidated) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            match iface.as_str() {
                DEVICE_INTERFACE => Some(Signal::DeviceChanged(address(&path), changed)),
                ADAPTER_INTERFACE => {
                    let discovering = bool::try_from(changed.get("Discovering")?.clone()).ok()?;
                    Some(Signal::Discovering(path, discovering))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn handle(signal: Signal, now: Duration, heard: &mut HashMap<String, Heard>) {
    match signal {
        Signal::Discovering(adapter, on) => {
            println!("{} {adapter}: discovering={on}", secs(now));
        }
        Signal::Added(addr, props) => {
            if let Some(line) = proximity(&props) {
                println!("{} {addr}: added, {line}", secs(now));
                entry(heard, &addr, now).payloads += 1;
            }
            // Every device is announced with `Connected`; only one that comes
            // connected says anything.
            if props
                .get("Connected")
                .and_then(|v| bool::try_from(v.clone()).ok())
                == Some(true)
            {
                connected(&addr, &props, now);
            }
        }
        Signal::Removed(addr) => {
            if let Some(h) = heard.get_mut(&addr) {
                println!("{} {addr}: removed", secs(now));
                h.removed += 1;
            }
        }
        Signal::DeviceChanged(addr, props) => {
            if let Some(line) = proximity(&props) {
                println!("{} {addr}: payload, {line}", secs(now));
                entry(heard, &addr, now).payloads += 1;
            }
            if props.contains_key("RSSI")
                && let Some(h) = heard.get_mut(&addr)
            {
                h.rssi += 1;
                h.last = now;
            }
            connected(&addr, &props, now);
        }
    }
}

/// Model, status byte and length of a proximity pairing payload among
/// `props`, if one is there and parses.
fn proximity(props: &HashMap<String, OwnedValue>) -> Option<String> {
    let mfg = props.get("ManufacturerData")?;
    let map = <HashMap<u16, OwnedValue>>::try_from(mfg.clone()).ok()?;
    let apple = <Vec<u8>>::try_from(map.get(&APPLE_COMPANY_ID)?.clone()).ok()?;
    let data = parse_proximity_data(&apple).ok()?;
    Some(format!(
        "model=0x{:04X} status=0x{:02X} {}B",
        data.product_id,
        data.status,
        apple.len()
    ))
}

fn connected(addr: &str, props: &HashMap<String, OwnedValue>, now: Duration) {
    if let Some(on) = props
        .get("Connected")
        .and_then(|v| bool::try_from(v.clone()).ok())
    {
        println!("{} {addr}: Connected={on}", secs(now));
    }
}

fn entry<'a>(heard: &'a mut HashMap<String, Heard>, addr: &str, now: Duration) -> &'a mut Heard {
    let h = heard.entry(addr.to_string()).or_insert_with(|| Heard {
        first: now,
        ..Heard::default()
    });
    h.last = now;
    h
}

fn summary(heard: &HashMap<String, Heard>, ran: Duration) {
    println!("\nsummary after {}s", secs(ran));
    let mut addrs: Vec<_> = heard.iter().collect();
    addrs.sort_by_key(|(_, h)| h.first);
    for (addr, h) in addrs {
        let span = (h.last - h.first).as_secs_f64();
        let rate = if span > 0.0 {
            f64::from(h.rssi) / span
        } else {
            0.0
        };
        println!(
            "{addr}: {}s to {}s, {} payloads, {} RSSI ({rate:.1}/s), {} removed, {} found, {} lost",
            secs(h.first),
            secs(h.last),
            h.payloads,
            h.rssi,
            h.removed,
            h.found,
            h.lost,
        );
    }
}

/// /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF -> AA:BB:CC:DD:EE:FF; any other path
/// as it is.
fn address(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|tail| tail.strip_prefix("dev_"))
        .map_or_else(|| path.to_string(), |hex| hex.replace('_', ":"))
}

fn secs(d: Duration) -> String {
    format!("{:7.3}", d.as_secs_f64())
}
