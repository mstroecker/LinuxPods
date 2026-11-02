//! BLE advertisement scanning via BlueZ D-Bus.
//!
//! Takes the Apple manufacturer data (company ID 0x004C) of each advertisement
//! BlueZ signals on org.bluez.Device1 and parses proximity pairing
//! advertisements out of it. An address heard for the first time becomes a
//! device, announced by the ObjectManager's InterfacesAdded with that first
//! advertisement among its properties; every later one comes as a
//! PropertiesChanged, which carries `ManufacturerData` only when the
//! payload changed, and `RSSI` alone otherwise. Both are read: watching
//! PropertiesChanged alone lost the first advertisement from every address -
//! each pod's, every new one after the lid has been closed, and every one
//! coming back after BlueZ dropped it 30 s into a silence (#86).
//!
//! A device BlueZ has already when discovery starts is announced to no one,
//! and its payload, unchanged, comes as `RSSI` alone: signals alone heard
//! nothing from it until its payload changed or its address did. BlueZ keeps
//! a pod's device for 30 s after it was last heard, discovering or not, and
//! another client's discovery keeps it heard (#153). So each time discovery
//! starts on an adapter, [`Scanning`] reads the devices BlueZ has there with
//! `GetManagedObjects`, and takes in what they hold as if it had been
//! signaled. One with an `RSSI` was heard in the discovery under way, and is
//! taken at once. One without was not - bluetoothd invalidates the strength
//! as discovery ends - and its data may be from long ago: it waits until
//! bluetoothd signals its `RSSI` back, which says the device was heard again
//! with that same data, as a change would come with it. How a read and the
//! signals around it keep their order is [`Waiting`]'s.
//!
//! Discovery runs on every powered adapter, whatever it is called (#51), and
//! starts again on each as it powers on, is plugged in, or bluetoothd restarts
//! (#50) - see [`Scanning`]. The signals come from devices under any of them,
//! as the subscription takes in every signal bluetoothd sends.
//!
//! Every advertisement that parses goes to the coordinator, which keeps only
//! those it identifies: from an address a stored IRK resolves, decrypted by the
//! same pair's ENC_KEY. The keys are what say which pair sent one.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, ready};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zbus::fdo::ManagedObjects;
use zbus::message::Sequence;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream, proxy};

use super::parser::{ProximityData, parse_proximity_data};
use crate::bluez::{self, AdapterSet, BLUEZ_SERVICE};

/// The key of Apple's entry in a device's `ManufacturerData`.
pub(crate) const APPLE_COMPANY_ID: u16 = 0x004C;

const DEVICE_INTERFACE: &str = "org.bluez.Device1";

/// On every adapter's object.
#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
}

/// One parsed advertisement plus the (randomized) MAC it arrived from.
#[derive(Debug, Clone)]
pub struct Advertisement {
    pub data: ProximityData,
    /// The randomized BLE MAC, not the pair's real one.
    pub ble_mac: Address,
}

pub struct Scanner {
    conn: Connection,
}

impl Scanner {
    /// A scanner on the system bus, where BlueZ is.
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("failed to connect to system bus")?;
        Ok(Self::with_connection(conn))
    }

    /// A scanner on `conn`, for whichever BlueZ answers there - under the test
    /// harness, a fake on a private bus (#72).
    pub fn with_connection(conn: Connection) -> Self {
        Self { conn }
    }

    /// Starts scanning on every powered adapter BlueZ has. One that fails is
    /// logged, and the others scan all the same. Returns the adapters now
    /// scanning, by path.
    ///
    /// Subscribe to [`Scanner::advertisements`] first: an address's first
    /// advertisement comes in the InterfacesAdded that announces its device,
    /// and one sent before the subscription is lost.
    ///
    /// Fails when BlueZ cannot be asked for its adapters, or scanning started
    /// on none of them.
    ///
    /// Once, for a probe or a test: discovery started here ends with the
    /// first power cycle, and a device BlueZ has already is heard only once
    /// its payload changes (#153). The app scans with [`Scanner::scan`],
    /// which starts it again, and reads those devices each time.
    pub async fn start_discovery(&self) -> Result<Vec<String>> {
        let adapters = bluez::adapters(&self.conn)
            .await
            .context("failed to list Bluetooth adapters")?;
        anyhow::ensure!(!adapters.is_empty(), "BlueZ has no Bluetooth adapter");

        let mut scanning = Vec::new();
        for adapter in adapters {
            // bluetoothd refuses discovery on an adapter that is off.
            if !adapter.powered {
                tracing::info!("not scanning on {}: it is powered off", adapter.path);
                continue;
            }
            match self.start_discovery_on(&adapter.path).await {
                Ok(()) => {
                    tracing::info!("BLE discovery started on {}", adapter.path);
                    scanning.push(adapter.path);
                }
                Err(e) => {
                    tracing::warn!("failed to start BLE discovery on {}: {e:#}", adapter.path)
                }
            }
        }
        anyhow::ensure!(!scanning.is_empty(), "no Bluetooth adapter is scanning");
        Ok(scanning)
    }

    /// Starts scanning on every powered adapter, and keeps it going on each
    /// as adapters change - see [`Scanning`]. Back once discovery has been
    /// asked for on every adapter powered now; there may be none.
    ///
    /// Subscribed to advertisements first, then to the adapters, so that
    /// nothing falls in between - see [`Scanner::start_discovery`].
    ///
    /// Fails only when either subscription does.
    pub async fn scan(self) -> Result<Scanning> {
        let (adverts, reads) = self.signals().await?;
        let adapters = bluez::watch_adapters(&self.conn)
            .await
            .context("failed to watch Bluetooth adapters")?;
        let none = {
            let adapters = adapters.borrow();
            adapters.on_bus() && adapters.is_empty()
        };
        if none {
            tracing::info!("found no Bluetooth adapter to scan on; waiting for one");
        }
        let mut scanning = Scanning {
            scanner: self,
            adverts: Box::pin(adverts),
            reads,
            adapters,
            discovery: HashMap::new(),
            bluez_gone: false,
        };
        scanning.update().await;
        Ok(scanning)
    }

    /// Sets an LE-only discovery filter on the adapter at `adapter`, and
    /// starts scanning there.
    pub(crate) async fn start_discovery_on(&self, adapter: &str) -> Result<()> {
        self.start_discovery_by(adapter).await.map(drop)
    }

    /// [`Scanner::start_discovery_on`], returning the unique name of the
    /// bluetoothd that started it: its reply's sender.
    async fn start_discovery_by(&self, adapter: &str) -> Result<String> {
        let adapter = Adapter1Proxy::new(&self.conn, adapter).await?;

        let mut filter = HashMap::new();
        filter.insert("Transport", Value::from("le"));
        adapter
            .set_discovery_filter(filter)
            .await
            .context("failed to set discovery filter")?;

        let reply = adapter
            .inner()
            .call_method("StartDiscovery", &())
            .await
            .context("failed to start discovery")?;
        let owner = reply
            .header()
            .sender()
            .context("BlueZ's reply names no sender")?
            .to_string();
        Ok(owner)
    }

    /// Stops this connection's discovery on the adapter at `adapter`. BlueZ
    /// refuses it when the connection has none there.
    async fn stop_discovery_on(&self, adapter: &str) -> Result<()> {
        Adapter1Proxy::new(&self.conn, adapter)
            .await?
            .stop_discovery()
            .await
            .context("failed to stop discovery")?;
        Ok(())
    }

    /// Yields advertisements as they arrive. Replaces the Go version's
    /// ScanForAirPods(timeout) polling call with a stream the coordinator drives.
    ///
    /// The stream may be read at the caller's pace, with calls to BlueZ on the
    /// scanner's connection in between - [`Scanning`] makes them whenever
    /// discovery has to start again. A task of its own drains the subscription as
    /// the signals arrive, as [`crate::bluez::drain`] does: left in zbus's
    /// queue, 64 advertisements would stop zbus reading the connection, and
    /// the reply to such a call would never come. What waits for the reader
    /// is the latest advertisement from each address - see [`Latest`]. The
    /// task ends with the connection, or when the stream is dropped.
    pub async fn advertisements(
        &self,
    ) -> Result<impl futures_util::Stream<Item = Advertisement> + use<>> {
        Ok(self.signals().await?.0)
    }

    /// [`Scanner::advertisements`], and the way in for what [`Scanning`]
    /// reads of BlueZ's devices.
    async fn signals(&self) -> Result<(impl Stream<Item = Advertisement> + use<>, Reads)> {
        // One rule, so one subscription: of two, the first's signals would go
        // unread while the bus confirmed the second, and 64 of them would stop
        // zbus reading the connection before the drain task began. It takes
        // in every signal bluetoothd sends - InterfacesAdded from its
        // ObjectManager at `/`, PropertiesChanged from each device - and one
        // stream keeps them in the order they were sent. zbus cannot check a
        // well-known sender, so whatever signal the connection gets comes
        // through: `device_properties` drops all but the two it reads by
        // their headers, before reading a body.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)?
            .build();
        let signals = MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .context("failed to subscribe to BlueZ's signals")?;

        let signals = signals.filter_map(|msg| async move { device_signal(&msg.ok()?) });
        Ok(drain(signals))
    }
}

/// What a signal said of the device at `path`, from [`device_signal`].
#[derive(Debug)]
struct Signal<At> {
    path: String,
    heard: Heard,
    /// Where the scanner's connection received it, as
    /// [`Message::recv_position`] has it: a read of BlueZ's devices is put
    /// in order with the signals by its reply's - see [`Waiting`].
    at: At,
    /// The unique name of its sender.
    sender: Option<String>,
}

/// What a device's signal says.
#[derive(Debug)]
enum Heard {
    /// Its data: the advertisement it carries, `None` if none parses - its
    /// data is not Apple's, or not proximity pairing - or if the device was
    /// announced with none.
    Data(Option<Advertisement>),
    /// Its strength alone: bluetoothd heard it again, and its data is
    /// unchanged.
    Again,
}

/// What a BlueZ signal says of a device - see [`Heard`] - or `None` for any
/// other signal, and a device's that says neither.
fn device_signal(msg: &Message) -> Option<Signal<Sequence>> {
    let (path, properties, announced) = device_properties(msg)?;
    let heard = if announced || properties.contains_key("ManufacturerData") {
        Heard::Data(advertisement(&path, &properties))
    } else if properties.contains_key("RSSI") {
        Heard::Again
    } else {
        return None;
    };
    Some(Signal {
        path,
        heard,
        at: msg.recv_position(),
        sender: msg.header().sender().map(|sender| sender.to_string()),
    })
}

/// The proximity pairing advertisement among the properties of the device at
/// `path`, if they carry one that parses.
fn advertisement(path: &str, properties: &HashMap<String, OwnedValue>) -> Option<Advertisement> {
    let apple_data = apple_manufacturer_data(properties)?;
    let Some(ble_mac) = mac_from_path(path) else {
        tracing::debug!("BLE advertisement dropped: {path} names no device address");
        return None;
    };

    // Logged here rather than downstream so it covers every advertisement
    // that parses, including ones the coordinator later drops: those it
    // does not identify as a pair's.
    match parse_proximity_data(&apple_data) {
        Ok(data) => {
            tracing::debug!(
                "BLE parsable: {ble_mac} model=0x{:04X} payload={}B",
                data.product_id,
                data.raw_data.len()
            );
            // Every parsable advertisement, including the ones the
            // coordinator drops as undecryptable - which is the only
            // place to see their payload.
            tracing::trace!("BLE parsable raw: {ble_mac} {}", crate::hex(&data.raw_data));
            Some(Advertisement { data, ble_mac })
        }
        Err(e) => {
            // Apple broadcasts plenty of non-proximity message types; at
            // debug level these would drown out everything else.
            tracing::trace!("BLE unparsable from {ble_mac}: {e}");
            None
        }
    }
}

/// A device BlueZ had when discovery started, as [`known_devices`] reads it.
#[derive(Debug)]
struct Known {
    path: String,
    advert: Advertisement,
    /// Whether it has an `RSSI`: whether bluetoothd heard it in the
    /// discovery under way.
    rssi: bool,
}

/// Picks out of a GetManagedObjects reply the devices on `adapters`, by
/// their `Adapter`, whose data is a proximity pairing advertisement, in the
/// order of their paths.
fn known_devices(objects: &ManagedObjects, adapters: &[&str]) -> Vec<Known> {
    let mut known: Vec<Known> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let device = interfaces.get(DEVICE_INTERFACE)?;
            let adapter = OwnedObjectPath::try_from(device.get("Adapter")?.clone()).ok()?;
            if !adapters.contains(&adapter.as_str()) {
                return None;
            }
            Some(Known {
                path: path.to_string(),
                advert: advertisement(path.as_str(), device)?,
                rssi: device.contains_key("RSSI"),
            })
        })
        .collect();
    known.sort_by(|a, b| a.path.cmp(&b.path));
    known
}

/// Scanning on every powered adapter, from [`Scanner::scan`], for as long as
/// the scanner's connection lasts (#50).
///
/// Discovery is BlueZ's to end: an adapter powering off - the desktop's
/// Bluetooth switch, rfkill, a suspend - ends it there, and so does one
/// unplugged, or bluetoothd restarting. So it is started again on each adapter
/// whenever [`AdapterSet`] says what was started there is gone: on one that
/// powers on, is plugged in, or is there after bluetoothd restarted. A refusal
/// is tried again after a while, [`bluez::backoff`] longer each time in a row.
/// The advertisement stream is the same throughout: subscribed once, ahead of
/// every start, so that none of them leaves a gap. After every start the
/// devices BlueZ has on the adapter are read, as nothing announces them
/// (#153) - see [`Waiting`].
///
/// Its own connection's discovery: bluetoothd keeps discovery by client.
pub struct Scanning {
    scanner: Scanner,
    adverts: Pin<Box<dyn Stream<Item = Advertisement> + Send>>,
    /// Where what is read of BlueZ's devices goes in with `adverts`.
    reads: Reads,
    adapters: watch::Receiver<AdapterSet>,
    /// Where discovery stands on each adapter there is, by path.
    discovery: HashMap<String, Discovery>,
    /// Whether BlueZ was not on the bus at the last update.
    bluez_gone: bool,
}

/// Where discovery stands on one adapter - see
/// [`bluez::AdapterState::discovery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Discovery {
    /// The adapter is off.
    Off,
    /// Started under that token.
    On(u64),
    /// Refused under that token, `failures` times in a row, and to be tried
    /// again at `retry`.
    Failed {
        token: u64,
        failures: u32,
        retry: Instant,
    },
}

impl Scanning {
    /// The next advertisement from any adapter. Meanwhile discovery is
    /// started again wherever it has to be. `None` once the scanner's
    /// connection's signals have ended: nothing more is heard on it.
    pub async fn next(&mut self) -> Option<Advertisement> {
        loop {
            let retry = self
                .discovery
                .values()
                .filter_map(|discovery| match discovery {
                    Discovery::Failed { retry, .. } => Some(*retry),
                    _ => None,
                })
                .min();
            tokio::select! {
                advert = self.adverts.next() => return advert,
                changed = self.adapters.changed() => {
                    changed.ok()?;
                    self.update().await;
                }
                () = until(retry) => self.update().await,
            }
        }
    }

    /// Starts discovery on every adapter where it has to be, then reads the
    /// devices BlueZ has on those it started on, and forgets the adapters
    /// that are gone.
    async fn update(&mut self) {
        // Cloned out: the borrow holds the watch's lock.
        let adapters = self.adapters.borrow_and_update().clone();
        let gone = !adapters.on_bus();
        if gone && !self.bluez_gone {
            tracing::info!("BlueZ is not on the bus: no BLE discovery until it is");
            self.discovery.clear();
        }
        self.bluez_gone = gone;
        self.discovery.retain(|path, _| {
            let here = adapters.get(path).is_some();
            if !here {
                tracing::info!("{path} is gone, and BLE discovery there with it");
            }
            here
        });
        // Open once discovery is about to start somewhere; and the adapters
        // it started on, by path, with the bluetoothd that started it.
        let mut window = None;
        let mut on = Vec::new();
        for (path, adapter) in adapters.iter() {
            let was = self.discovery.get(path).copied();
            let Some(token) = adapter.discovery else {
                match was {
                    Some(Discovery::Off) => {}
                    Some(Discovery::On(_)) => {
                        tracing::info!("BLE discovery on {path} ended: it powered off");
                    }
                    _ => tracing::info!("not scanning on {path}: it is powered off"),
                }
                self.discovery.insert(path.to_string(), Discovery::Off);
                continue;
            };
            let failures = match was {
                Some(Discovery::On(on)) if on == token => continue,
                Some(Discovery::Failed {
                    token: failed,
                    failures,
                    retry,
                }) if failed == token => {
                    if retry > Instant::now() {
                        continue;
                    }
                    failures
                }
                _ => 0,
            };
            // What devices signal from here on is kept for the read, which
            // leaves to the signals what they say themselves.
            window.get_or_insert_with(|| self.reads.window());
            let mut started = self.scanner.start_discovery_by(path).await;
            // InProgress means one of two things, and BlueZ does not say
            // which: this connection discovers there already - a token can
            // change without BlueZ ending discovery, as when a re-read sees
            // an off-and-on it had seen already - or the start failed and it
            // does not. bluetoothd answers InProgress as well when the kernel
            // refuses its start as busy, and drops the caller as a discovery
            // client (`discovery_complete`, `start_discovery_complete` in
            // BlueZ 5.87's src/adapter.c); the kernel is busy so for seconds
            // after a `kill -9`, the dead bluetoothd's discovery still
            // running. Taken as started, nothing scanned again (#123). So
            // this connection's discovery there, if any, is stopped - BlueZ
            // refuses the stop where there is none, which changes nothing -
            // and asked for afresh; refused again, it is tried later like any
            // refusal. Not on the backoff alone: where discovery is ours,
            // every start is InProgress for as long as it runs.
            if let Err(e) = &started
                && is_in_progress(e)
            {
                tracing::debug!(
                    "BLE discovery on {path} in progress, or refused as busy; asking for it again"
                );
                let _ = self.scanner.stop_discovery_on(path).await;
                started = self.scanner.start_discovery_by(path).await;
            }
            let next = match started {
                Ok(owner) => {
                    let again = matches!(was, Some(Discovery::On(_)));
                    let started = if again { "restarted" } else { "started" };
                    tracing::info!("BLE discovery {started} on {path}");
                    on.push((path.to_string(), owner));
                    Discovery::On(token)
                }
                Err(e) => {
                    let failures = failures + 1;
                    let wait = bluez::backoff(failures);
                    tracing::warn!(
                        "failed to start BLE discovery on {path}: {e:#}; trying again in {}s",
                        wait.as_secs()
                    );
                    Discovery::Failed {
                        token,
                        failures,
                        retry: Instant::now() + wait,
                    }
                }
            };
            self.discovery.insert(path.to_string(), next);
        }
        if let Some(window) = window
            && !on.is_empty()
        {
            Self::read_known(&self.scanner.conn, window, &on).await;
        }
    }

    /// Reads the devices BlueZ has on the adapters discovery has just
    /// started on - `started`, each by path with the unique name of the
    /// bluetoothd that started it - and takes in what they hold, as
    /// [`Waiting::read`] has it, closing `window`.
    ///
    /// After the starts, so that a device heard in the discovery under way
    /// has its `RSSI`. Only where the bluetoothd that answers started
    /// discovery: one that has restarted in between has neither that
    /// discovery nor those devices, and the adapters' watch has discovery
    /// started there again, and read again.
    async fn read_known(conn: &Connection, window: Window, started: &[(String, String)]) {
        let reply = match conn
            .call_method(
                Some(BLUEZ_SERVICE),
                "/",
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            )
            .await
        {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!("failed to read the devices BlueZ has: {e}");
                return;
            }
        };
        let Some(owner) = reply.header().sender().map(|owner| owner.to_string()) else {
            return;
        };
        let objects = match reply.body().deserialize::<ManagedObjects>() {
            Ok(objects) => objects,
            Err(e) => {
                tracing::warn!("failed to read the devices BlueZ has: {e}");
                return;
            }
        };
        let adapters: Vec<&str> = started
            .iter()
            .filter(|(_, by)| *by == owner)
            .map(|(path, _)| path.as_str())
            .collect();
        let known = known_devices(&objects, &adapters);
        let (now, held) = window.read(reply.recv_position(), &owner, &adapters, known);
        if now + held > 0 {
            tracing::info!(
                "BLE: took in {now} advertisement(s) BlueZ had heard already; \
                 {held} more once heard again"
            );
        }
    }
}

/// Whether BlueZ refused discovery as in progress: started already by the
/// caller, or refused by a busy kernel - see [`Scanning::update`].
fn is_in_progress(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<zbus::Error>(),
        Some(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.bluez.Error.InProgress"
    )
}

/// Returns at `at`, or never.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Reads `signals` as they come, in a task of its own, into a [`Waiting`],
/// from where the returned stream takes the advertisements at its reader's
/// pace, as [`crate::bluez::drain`] does; and the [`Reads`] that put what is
/// read of BlueZ's devices in with them. The task ends with `signals`, or once
/// the stream is dropped; the stream ends once the task has and nothing is
/// waiting.
fn drain<S>(signals: S) -> (impl Stream<Item = Advertisement> + use<S>, Reads)
where
    S: Stream<Item = Signal<Sequence>> + Send + 'static,
{
    let waiting = Arc::new(Mutex::new(Waiting::default()));
    // Rung after every advertisement that comes in. It holds one ring at
    // most, so neither side ever waits on the reader. `Reads` holds it
    // weakly: the stream ends with the task.
    let (bell, mut rung) = mpsc::channel(1);
    let reads = Reads {
        waiting: waiting.clone(),
        bell: bell.downgrade(),
    };
    let queue = waiting.clone();
    tokio::spawn(async move {
        tokio::pin!(signals);
        loop {
            tokio::select! {
                () = bell.closed() => return,
                signal = signals.next() => match signal {
                    Some(signal) => {
                        if lock(&queue).signal(signal) {
                            let _ = bell.try_send(());
                        }
                    }
                    None => return,
                },
            }
        }
    });
    let adverts = futures_util::stream::poll_fn(move |cx| {
        loop {
            if let Some(advert) = lock(&waiting).latest.pop() {
                return Poll::Ready(Some(advert));
            }
            // Every advertisement rings after it, so with the task gone and
            // no ring left, nothing more is coming.
            if ready!(rung.poll_recv(cx)).is_none() {
                return Poll::Ready(None);
            }
        }
    });
    (adverts, reads)
}

/// Nothing in [`Waiting`] panics halfway, so a poisoned lock holds it whole.
fn lock<T>(waiting: &Mutex<T>) -> MutexGuard<'_, T> {
    waiting.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The way in for what is read of BlueZ's devices, beside the signals: see
/// [`Waiting`].
#[derive(Clone)]
struct Reads {
    waiting: Arc<Mutex<Waiting<Sequence>>>,
    bell: mpsc::WeakSender<()>,
}

impl Reads {
    /// Has what devices signal from here on kept, until the [`Window`] is
    /// read or dropped.
    fn window(&self) -> Window {
        lock(&self.waiting).open();
        Window(self.clone())
    }
}

/// Open from [`Reads::window`] until it is read, or dropped unread: a read
/// that failed, or was given up on.
struct Window(Reads);

impl Window {
    /// Takes in what BlueZ's devices on `adapters` held - see
    /// [`Waiting::read`] - and wakes the reader for it.
    fn read(
        self,
        at: Sequence,
        owner: &str,
        adapters: &[&str],
        known: Vec<Known>,
    ) -> (usize, usize) {
        let (now, held) = lock(&self.0.waiting).read(at, owner, adapters, known);
        if now > 0
            && let Some(bell) = self.0.bell.upgrade()
        {
            let _ = bell.try_send(());
        }
        (now, held)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        lock(&self.0.waiting).close();
    }
}

/// The advertisements waiting for the scanner's reader - see [`Latest`] - and
/// what reads of BlueZ's devices left to put in order with the signals.
///
/// A device BlueZ has when discovery starts is read from its
/// `GetManagedObjects` (#153), and its data taken in as the signals' is.
/// The read and the signals keep the order bluetoothd sent them in, so that
/// nothing is taken in out of order, nothing twice, and nothing lost:
///
/// - bluetoothd sends what it signals before a reply that shows it: its
///   gdbus flushes pending property signals before every reply
///   (`g_dbus_send_message` in BlueZ's `gdbus/object.c`). So a signal sent
///   before the reply says nothing newer than the reply does, and one sent
///   after it nothing older.
/// - The connection receives them in that order, and numbers them so:
///   [`Message::recv_position`]. Each signal carries its number `at`, and so
///   does the read, its reply's.
/// - The read waits for nothing the signals do: it is taken in as its reply
///   comes, while signals sent before it may not have come through the drain
///   task yet, and signals sent after it may have, the reader may even have
///   taken them. So [`Waiting::open`], before discovery is started and the
///   read asked for, has what each device signals from then on kept until
///   the read: a device that signaled data since is left to its signals,
///   which have told it, or something newer. Any other is taken in from the
///   read, and a signal from it sent before the read, still to come, is
///   dropped: what it says, the read said, or newer.
///
/// A device without an `RSSI` was not heard in the discovery under way, and
/// is held until bluetoothd signals its `RSSI`, which says it was heard again
/// with its data unchanged; one heard since the window opened is taken at
/// once. A signal counts against a read only from the bluetoothd that
/// answered it.
#[derive(Debug)]
struct Waiting<At> {
    latest: Latest,
    /// What each device signaled since the window opened, by path; `None`
    /// while none is open.
    since: Option<HashMap<String, Since>>,
    /// The devices a read took in or holds, by path, until their next
    /// signal from the bluetoothd that answered it - or, for those taken in,
    /// until every signal sent before the read has come.
    reads: HashMap<String, Read<At>>,
    /// The latest read with devices taken in: until a signal sent after it
    /// comes, one sent before it can still come.
    unsettled: Option<At>,
}

impl<At> Default for Waiting<At> {
    fn default() -> Self {
        Self {
            latest: Latest::default(),
            since: None,
            reads: HashMap::new(),
            unsettled: None,
        }
    }
}

/// What a device signaled since the window opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Since {
    /// Its data, perhaps with its strength.
    Data,
    /// Its strength alone.
    Again,
}

/// A device as a read left it.
#[derive(Debug)]
struct Read<At> {
    /// Where the read's reply was received.
    at: At,
    /// The unique name of the bluetoothd that answered it.
    owner: String,
    /// Its advertisement, held until it is heard again; `None` once taken
    /// in.
    held: Option<Advertisement>,
}

impl<At: Copy + Ord> Waiting<At> {
    /// Opens the window: what devices signal from here on is kept, for the
    /// next [`Waiting::read`].
    fn open(&mut self) {
        self.since.get_or_insert_with(HashMap::new);
    }

    /// Closes the window, read or not.
    fn close(&mut self) {
        self.since = None;
    }

    /// Takes in a signal. Returns whether an advertisement came in.
    fn signal(&mut self, signal: Signal<At>) -> bool {
        // Signals come in the order they were received: one sent after the
        // read leaves none sent before it to come.
        if self.unsettled.is_some_and(|read| signal.at > read) {
            self.reads.retain(|_, read| read.held.is_some());
            self.unsettled = None;
        }
        let read = match self.reads.get(&signal.path) {
            Some(read) if signal.sender.as_deref() == Some(read.owner.as_str()) => {
                if signal.at < read.at {
                    return false;
                }
                self.reads.remove(&signal.path)
            }
            _ => None,
        };
        let advert = match signal.heard {
            Heard::Data(advert) => {
                if let Some(since) = &mut self.since {
                    since.insert(signal.path, Since::Data);
                }
                advert
            }
            Heard::Again => {
                if let Some(since) = &mut self.since {
                    since.entry(signal.path).or_insert(Since::Again);
                }
                read.and_then(|read| read.held)
            }
        };
        let Some(advert) = advert else {
            return false;
        };
        self.latest.push(advert);
        true
    }

    /// Takes in what BlueZ's devices on `adapters` held - `known`, as
    /// [`known_devices`] reads them - as read in a reply received `at`, from
    /// `owner`, and closes the window. Returns how many were taken in, and
    /// how many are held until heard again.
    ///
    /// A device that signaled its data since the window opened is left out:
    /// its signals have told it, or something newer. Any other is taken in if
    /// it has an `RSSI`, or signaled one since the window opened; if not, it
    /// is held. A device held from an earlier read of these adapters and not
    /// in this one is let go: BlueZ has it no more.
    fn read(
        &mut self,
        at: At,
        owner: &str,
        adapters: &[&str],
        known: Vec<Known>,
    ) -> (usize, usize) {
        let since = self.since.take().unwrap_or_default();
        self.reads.retain(|path, read| {
            let adapter = path.rsplit_once('/').map_or("", |(adapter, _)| adapter);
            read.held.is_none() || !adapters.contains(&adapter)
        });
        let (mut now, mut held) = (0, 0);
        for known in known {
            let heard = match since.get(&known.path) {
                Some(Since::Data) => continue,
                Some(Since::Again) => true,
                None => known.rssi,
            };
            let advert = if heard {
                self.latest.push(known.advert);
                now += 1;
                None
            } else {
                held += 1;
                Some(known.advert)
            };
            let read = Read {
                at,
                owner: owner.to_string(),
                held: advert,
            };
            self.reads.insert(known.path, read);
        }
        if now > 0 {
            self.unsettled = Some(self.unsettled.map_or(at, |read| read.max(at)));
        }
        (now, held)
    }
}

/// The advertisements waiting for the scanner's reader: the latest from each
/// address, in the order each address first came since it was last taken.
///
/// An advertiser sends as often as it likes, and a flood brings new addresses
/// as fast as it likes. Queued one per signal, they would pile up without
/// bound behind a reader that cannot keep up. An address's latest
/// advertisement says all its earlier ones did, as each carries the device's
/// whole state, so keeping only that one loses nothing. No address is dropped:
/// the one dropped could be a pair's.
#[derive(Debug, Default)]
struct Latest {
    /// Addresses with an advertisement waiting, oldest first.
    order: VecDeque<Address>,
    /// The latest advertisement from each address in `order`.
    adverts: HashMap<Address, Advertisement>,
}

impl Latest {
    fn push(&mut self, advert: Advertisement) {
        let address = advert.ble_mac;
        if self.adverts.insert(address, advert).is_none() {
            self.order.push_back(address);
        }
    }

    fn pop(&mut self) -> Option<Advertisement> {
        let address = self.order.pop_front()?;
        self.adverts.remove(&address)
    }
}

/// The path of the device a BlueZ signal is about, the Device1 properties it
/// carries, and whether it announces the device: all of them in the
/// InterfacesAdded that announces a new device, the changed ones in a
/// PropertiesChanged. `None` for any other signal.
///
/// InterfacesAdded comes from the ObjectManager and names the device in its
/// body; PropertiesChanged comes from the device itself. Either way the device
/// is under `/org/bluez`, where BlueZ keeps its adapters and their devices.
fn device_properties(msg: &Message) -> Option<(String, HashMap<String, OwnedValue>, bool)> {
    let under_bluez = |path: &str| path.starts_with("/org/bluez/");
    let header = msg.header();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path = header.path()?.to_string();
            if !under_bluez(&path) {
                return None;
            }
            let (iface, changed, _invalidated) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            (iface == DEVICE_INTERFACE).then_some((path, changed, false))
        }
        ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
            let (path, mut interfaces) = msg
                .body()
                .deserialize::<(
                    OwnedObjectPath,
                    HashMap<String, HashMap<String, OwnedValue>>,
                )>()
                .ok()?;
            if !under_bluez(path.as_str()) {
                return None;
            }
            Some((path.to_string(), interfaces.remove(DEVICE_INTERFACE)?, true))
        }
        _ => None,
    }
}

/// Extracts the Apple (0x004C) entry out of a ManufacturerData property.
fn apple_manufacturer_data(properties: &HashMap<String, OwnedValue>) -> Option<Vec<u8>> {
    let mfg = properties.get("ManufacturerData")?;
    let map = <HashMap<u16, OwnedValue>>::try_from(mfg.clone()).ok()?;
    let apple = map.get(&APPLE_COMPANY_ID)?;
    <Vec<u8>>::try_from(apple.clone()).ok()
}

/// /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF -> AA:BB:CC:DD:EE:FF, under any
/// adapter.
fn mac_from_path(path: &str) -> Option<Address> {
    let tail = path.rsplit('/').next()?;
    let hex = tail.strip_prefix("dev_")?;
    hex.replace('_', ":").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mac_from_device_path() {
        let mac = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(
            mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
            Some(mac)
        );
        assert_eq!(
            mac_from_path("/org/bluez/hci0/dev_aa_bb_cc_dd_ee_ff"),
            Some(mac),
            "the spelling does not matter"
        );
        assert_eq!(
            mac_from_path("/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF"),
            Some(mac),
            "nor the adapter"
        );
    }

    /// An advertisement from `address`, told apart by `status`.
    fn advert(address: u8, status: u8) -> Advertisement {
        Advertisement {
            data: ProximityData {
                status,
                ..ProximityData::default()
            },
            ble_mac: Address::new([0x4A, 0, 0, 0, 0, address]),
        }
    }

    /// What `latest` gives out, as (address, status).
    fn take_all(latest: &mut Latest) -> Vec<(u8, u8)> {
        std::iter::from_fn(|| latest.pop())
            .map(|a| (a.ble_mac.0[5], a.data.status))
            .collect()
    }

    /// An address advertising over and over while the reader is busy is one
    /// advertisement waiting, the latest; once that is taken, the next is
    /// waiting again.
    #[test]
    fn an_address_has_its_latest_advertisement_waiting() {
        let mut latest = Latest::default();
        for status in 0..100 {
            latest.push(advert(1, status));
        }
        assert_eq!(take_all(&mut latest), [(1, 99)]);

        latest.push(advert(1, 100));
        assert_eq!(take_all(&mut latest), [(1, 100)]);
    }

    /// Every address keeps its own, in the order it first came: a flood of new
    /// addresses loses none of them.
    #[test]
    fn addresses_keep_their_advertisements_apart() {
        let mut latest = Latest::default();
        for address in 0..=255 {
            latest.push(advert(address, 0));
        }
        latest.push(advert(7, 1));
        let taken = take_all(&mut latest);
        assert_eq!(taken.len(), 256);
        assert_eq!(taken[..3], [(0, 0), (1, 0), (2, 0)]);
        assert_eq!(taken[7], (7, 1), "the latest, where the address first came");
    }

    /// A device at a random address, as an advertisement comes from.
    const DEVICE: &str = "/org/bluez/hci0/dev_5C_4D_3F_B5_41_B6";

    /// Device1 properties carrying `apple` as Apple's manufacturer data.
    fn device1(apple: &[u8]) -> HashMap<&'static str, Value<'static>> {
        let mfg = HashMap::from([(APPLE_COMPANY_ID, Value::from(apple.to_vec()))]);
        HashMap::from([
            ("Address", Value::from("5C:4D:3F:B5:41:B6")),
            ("ManufacturerData", Value::from(mfg)),
        ])
    }

    /// InterfacesAdded from the ObjectManager at `/`, announcing `object`
    /// with `interfaces`.
    fn added(object: &str, interfaces: HashMap<&str, HashMap<&str, Value<'_>>>) -> Message {
        let object = zbus::zvariant::ObjectPath::try_from(object).unwrap();
        Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
            .unwrap()
            .build(&(object, interfaces))
            .unwrap()
    }

    /// PropertiesChanged from `object`, of `changed` on `iface`.
    fn changed(object: &str, iface: &str, changed: HashMap<&str, Value<'_>>) -> Message {
        Message::signal(
            object,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap()
        .build(&(iface, changed, Vec::<&str>::new()))
        .unwrap()
    }

    /// Regression (#86): only PropertiesChanged was read, and an address's
    /// first advertisement - which comes with its device, in InterfacesAdded -
    /// was lost. The device is named in the body, not the header.
    #[test]
    fn reads_the_advertisement_a_new_device_is_announced_with() {
        let msg = added(
            DEVICE,
            HashMap::from([(DEVICE_INTERFACE, device1(&[0x07, 0x19]))]),
        );

        let (path, properties, announced) = device_properties(&msg).unwrap();
        assert_eq!(path, DEVICE);
        assert_eq!(apple_manufacturer_data(&properties), Some(vec![0x07, 0x19]));
        assert!(announced);
    }

    #[test]
    fn reads_the_advertisement_a_device_changes_with() {
        let msg = changed(DEVICE, DEVICE_INTERFACE, device1(&[0x07, 0x19]));

        let (path, properties, announced) = device_properties(&msg).unwrap();
        assert_eq!(path, DEVICE);
        assert_eq!(apple_manufacturer_data(&properties), Some(vec![0x07, 0x19]));
        assert!(!announced);
    }

    /// Only a device's own properties count, and only BlueZ's devices.
    #[test]
    fn ignores_what_is_not_a_device() {
        let battery = HashMap::from([("Percentage", Value::from(80u8))]);
        assert!(
            device_properties(&added(
                DEVICE,
                HashMap::from([("org.bluez.Battery1", battery.clone())])
            ))
            .is_none(),
            "an interface added to a device"
        );
        assert!(
            device_properties(&changed(DEVICE, "org.bluez.Battery1", battery)).is_none(),
            "another interface's properties"
        );
        assert!(
            device_properties(&added(
                "/org/example/dev_5C_4D_3F_B5_41_B6",
                HashMap::from([(DEVICE_INTERFACE, device1(&[0x07]))])
            ))
            .is_none(),
            "an object outside /org/bluez"
        );
        assert!(
            device_properties(&changed(
                "/org/example/dev_5C_4D_3F_B5_41_B6",
                DEVICE_INTERFACE,
                device1(&[0x07])
            ))
            .is_none(),
            "a change outside /org/bluez"
        );

        // The rest of what the subscription takes in goes by its headers.
        let acquired = Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameAcquired",
        )
        .unwrap()
        .build(&(":1.7",))
        .unwrap();
        assert!(device_properties(&acquired).is_none());
        let removed = Message::signal(
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesRemoved",
        )
        .unwrap()
        .build(&(
            zbus::zvariant::ObjectPath::try_from(DEVICE).unwrap(),
            vec![DEVICE_INTERFACE],
        ))
        .unwrap();
        assert!(device_properties(&removed).is_none());
    }

    #[test]
    fn rejects_non_device_paths() {
        assert_eq!(mac_from_path("/org/bluez/hci0"), None);
        assert_eq!(mac_from_path("/org/bluez/hci1"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_TOO_SHORT"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_GG"), None);
        assert_eq!(mac_from_path(""), None);
    }

    /// The Apple data of a proximity pairing advertisement that parses, its
    /// status byte `status`.
    fn proximity(status: u8) -> Vec<u8> {
        let mut data = vec![0x07, 0x19, 0x01, 0x27, 0x20, status];
        data.resize(2 + 0x19, 0);
        data
    }

    /// A GetManagedObjects reply, of `objects` each with `interfaces`.
    fn managed<'a>(
        objects: impl IntoIterator<Item = (&'a str, Vec<(&'a str, HashMap<&'a str, Value<'a>>)>)>,
    ) -> ManagedObjects {
        objects
            .into_iter()
            .map(|(path, interfaces)| {
                let interfaces = interfaces
                    .into_iter()
                    .map(|(name, props)| {
                        let props = props
                            .into_iter()
                            .map(|(key, value)| (key.to_string(), value.try_into_owned().unwrap()))
                            .collect();
                        (name.try_into().unwrap(), props)
                    })
                    .collect();
                (path.try_into().unwrap(), interfaces)
            })
            .collect()
    }

    /// A device's properties: on `adapter`, with `data` as its Apple data and
    /// `rssi` if any.
    fn device_on(
        adapter: &str,
        data: Option<Vec<u8>>,
        rssi: Option<i16>,
    ) -> Vec<(&'static str, HashMap<&'static str, Value<'static>>)> {
        let adapter = zbus::zvariant::ObjectPath::try_from(adapter.to_string()).unwrap();
        let mut props = HashMap::from([("Adapter", Value::from(adapter))]);
        if let Some(data) = data {
            let mfg = HashMap::from([(APPLE_COMPANY_ID, Value::from(data))]);
            props.insert("ManufacturerData", Value::from(mfg));
        }
        if let Some(rssi) = rssi {
            props.insert("RSSI", Value::from(rssi));
        }
        vec![(DEVICE_INTERFACE, props)]
    }

    /// From a GetManagedObjects reply, the devices on the adapters discovery
    /// started on, by their `Adapter`, whose data is a proximity pairing
    /// advertisement, each with whether it has its strength (#153).
    #[test]
    fn known_devices_are_the_proximity_advertisers_on_the_adapters_started() {
        let hci0 = "/org/bluez/hci0";
        let other_company = {
            let mut props = device_on(hci0, None, Some(-60));
            let mfg = HashMap::from([(0x0006u16, Value::from(proximity(1)))]);
            props[0].1.insert("ManufacturerData", Value::from(mfg));
            props
        };
        let objects = managed([
            (
                "/org/bluez",
                vec![("org.bluez.AgentManager1", HashMap::new())],
            ),
            (hci0, vec![("org.bluez.Adapter1", HashMap::new())]),
            (
                "/org/bluez/hci0/dev_4A_00_00_00_00_02",
                device_on(hci0, Some(proximity(2)), None),
            ),
            (
                "/org/bluez/hci0/dev_4A_00_00_00_00_01",
                device_on(hci0, Some(proximity(1)), Some(-60)),
            ),
            (
                "/org/bluez/hci1/dev_4A_00_00_00_00_03",
                device_on("/org/bluez/hci1", Some(proximity(3)), Some(-60)),
            ),
            (
                "/org/bluez/hci0/dev_4A_00_00_00_00_04",
                device_on(hci0, Some(vec![0x10, 0x05, 0x01]), Some(-60)),
            ),
            (
                "/org/bluez/hci0/dev_4A_00_00_00_00_05",
                device_on(hci0, None, Some(-60)),
            ),
            ("/org/bluez/hci0/dev_4A_00_00_00_00_06", other_company),
            (
                "/org/bluez/hci0/dev_4A_00_00_00_00_07",
                vec![(
                    DEVICE_INTERFACE,
                    HashMap::from([("RSSI", Value::from(-60i16))]),
                )],
            ),
        ]);

        let known = known_devices(&objects, &[hci0]);
        let picked: Vec<(u8, u8, bool)> = known
            .iter()
            .map(|k| (k.advert.ble_mac.0[5], k.advert.data.status, k.rssi))
            .collect();
        assert_eq!(picked, [(1, 1, true), (2, 2, false)]);
        assert_eq!(known[0].path, "/org/bluez/hci0/dev_4A_00_00_00_00_01");
        assert_eq!(
            known_devices(&objects, &[hci0, "/org/bluez/hci1"]).len(),
            3,
            "and the second adapter's, once started there"
        );
        assert!(known_devices(&objects, &[]).is_empty());
    }

    /// The bluetoothd that answers, and another sender.
    const OWNER: &str = ":1.7";
    const STRANGER: &str = ":1.99";

    /// The path of the device at the address [`advert`] makes of `address`,
    /// on the adapter called `adapter`.
    fn path(adapter: &str, address: u8) -> String {
        format!("/org/bluez/{adapter}/dev_4A_00_00_00_00_{address:02X}")
    }

    /// A signal from the device at `address` on hci0, received `at`, from
    /// `sender`: its data, an advertisement told apart by `status`, or with
    /// `None` its strength alone.
    fn signal(address: u8, status: Option<u8>, at: u32, sender: &str) -> Signal<u32> {
        Signal {
            path: path("hci0", address),
            heard: match status {
                Some(status) => Heard::Data(Some(advert(address, status))),
                None => Heard::Again,
            },
            at,
            sender: Some(sender.to_string()),
        }
    }

    /// The device at `address` on `adapter` as a read finds it: with its
    /// advertisement told apart by `status`, and its strength or not.
    fn known(adapter: &str, address: u8, status: u8, rssi: bool) -> Known {
        Known {
            path: path(adapter, address),
            advert: advert(address, status),
            rssi,
        }
    }

    /// What waits for the reader, as (address, status).
    fn taken(waiting: &mut Waiting<u32>) -> Vec<(u8, u8)> {
        take_all(&mut waiting.latest)
    }

    /// A device heard in the discovery under way - it has its strength - is
    /// taken in from the read at once; one not heard since discovery last
    /// ended is held until bluetoothd hears it again, which it signals by
    /// the strength alone, and taken in then, once.
    #[test]
    fn a_read_takes_in_what_was_heard_and_holds_the_rest_until_heard_again() {
        let mut waiting = Waiting::default();
        waiting.open();
        let read = waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 1, true), known("hci0", 2, 2, false)],
        );
        assert_eq!(read, (1, 1));
        assert_eq!(taken(&mut waiting), [(1, 1)]);
        assert!(waiting.since.is_none(), "the window closed");

        assert!(!waiting.signal(signal(1, None, 11, OWNER)), "its strength");
        assert!(waiting.signal(signal(2, None, 12, OWNER)));
        assert_eq!(taken(&mut waiting), [(2, 2)], "heard again");
        assert!(!waiting.signal(signal(2, None, 13, OWNER)), "once");
        assert!(waiting.signal(signal(2, Some(3), 14, OWNER)));
        assert_eq!(taken(&mut waiting), [(2, 3)]);
    }

    /// A signal sent before the read, still on its way when the read is
    /// taken in, says nothing newer than the read: dropped, it cannot put an
    /// older payload over the read's. One sent after it is taken in.
    #[test]
    fn a_signal_sent_before_the_read_comes_after_it_and_is_dropped() {
        let mut waiting = Waiting::default();
        waiting.open();
        waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 3, true), known("hci0", 2, 2, false)],
        );
        assert!(!waiting.signal(signal(1, Some(1), 7, OWNER)));
        assert!(!waiting.signal(signal(1, Some(2), 8, OWNER)));
        assert!(!waiting.signal(signal(2, None, 9, OWNER)), "still held");
        assert_eq!(taken(&mut waiting), [(1, 3)], "the read's alone");

        assert!(waiting.signal(signal(1, Some(4), 11, OWNER)));
        assert_eq!(taken(&mut waiting), [(1, 4)]);
        assert!(waiting.signal(signal(2, None, 12, OWNER)));
        assert_eq!(taken(&mut waiting), [(2, 2)]);
    }

    /// Past the read, no signal sent before it can come: what it took in is
    /// forgotten, and what it holds is kept.
    #[test]
    fn what_a_read_took_in_is_forgotten_once_a_later_signal_comes() {
        let mut waiting = Waiting::default();
        waiting.open();
        waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 1, true), known("hci0", 2, 2, false)],
        );
        assert_eq!(waiting.reads.len(), 2);
        waiting.signal(signal(9, None, 11, OWNER));
        assert_eq!(
            waiting.reads.keys().collect::<Vec<_>>(),
            [&path("hci0", 2)],
            "the one held"
        );
        assert!(waiting.unsettled.is_none());
    }

    /// A device that signaled its data since the window opened is left to
    /// its signals: they told it, or something newer than the read, which
    /// the reader may have taken already. One heard again since, its
    /// strength alone, is taken in, though the read shows no strength.
    #[test]
    fn what_signals_told_since_the_window_opened_is_left_to_them() {
        let mut waiting = Waiting::default();
        waiting.open();
        assert!(
            waiting.signal(signal(1, Some(5), 12, OWNER)),
            "after the read"
        );
        assert_eq!(taken(&mut waiting), [(1, 5)]);
        assert!(!waiting.signal(signal(2, None, 13, OWNER)));
        let read = waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 4, true), known("hci0", 2, 2, false)],
        );
        assert_eq!(read, (1, 0));
        assert_eq!(taken(&mut waiting), [(2, 2)], "not the older payload");

        // Closed, and nothing is kept until it opens again.
        waiting.signal(signal(3, Some(1), 14, OWNER));
        assert!(waiting.since.is_none());
        waiting.open();
        waiting.close();
        assert!(waiting.since.is_none());
    }

    /// Only the bluetoothd that answered the read counts against it: a
    /// signal from anyone else neither releases a device held nor is dropped
    /// as older than the read.
    #[test]
    fn only_the_bluetoothd_read_from_counts_against_a_read() {
        let mut waiting = Waiting::default();
        waiting.open();
        waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 1, false)],
        );
        assert!(!waiting.signal(signal(1, None, 11, STRANGER)));
        assert!(waiting.signal(signal(1, Some(9), 5, STRANGER)), "as ever");
        assert_eq!(taken(&mut waiting), [(1, 9)]);
        assert!(waiting.signal(signal(1, None, 12, OWNER)));
        assert_eq!(taken(&mut waiting), [(1, 1)]);
    }

    /// Data signaled after the read replaces what it holds, and a device
    /// announced afresh is another: either way the hold goes.
    #[test]
    fn new_data_lets_go_of_what_a_read_holds() {
        let mut waiting = Waiting::default();
        waiting.open();
        waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0"],
            vec![known("hci0", 1, 1, false), known("hci0", 2, 2, false)],
        );
        assert!(waiting.signal(signal(1, Some(3), 11, OWNER)));
        let mut announced = signal(2, None, 12, OWNER);
        announced.heard = Heard::Data(None);
        assert!(!waiting.signal(announced));
        assert!(!waiting.signal(signal(1, None, 13, OWNER)));
        assert!(!waiting.signal(signal(2, None, 14, OWNER)));
        assert_eq!(taken(&mut waiting), [(1, 3)]);
    }

    /// A read of an adapter replaces what an earlier read of it holds - a
    /// device not in it BlueZ has no more - and leaves another adapter's.
    #[test]
    fn a_read_again_replaces_what_the_last_read_of_its_adapter_holds() {
        let mut waiting = Waiting::default();
        waiting.open();
        waiting.read(
            10,
            OWNER,
            &["/org/bluez/hci0", "/org/bluez/hci1"],
            vec![known("hci0", 1, 1, false), known("hci1", 2, 2, false)],
        );
        waiting.open();
        waiting.read(20, OWNER, &["/org/bluez/hci0"], vec![]);
        assert!(!waiting.signal(signal(1, None, 21, OWNER)), "let go of");
        let mut on_hci1 = signal(2, None, 22, OWNER);
        on_hci1.path = path("hci1", 2);
        assert!(waiting.signal(on_hci1));
        assert_eq!(taken(&mut waiting), [(2, 2)]);
    }

    /// A device's signal says its data when it carries any, or announces the
    /// device - whatever it holds; its strength alone, that it was heard
    /// again; and anything else nothing the scanner reads.
    #[test]
    fn a_device_signal_says_its_data_or_that_it_was_heard_again() {
        let heard = |msg: &Message| device_signal(msg).map(|signal| signal.heard);
        let rssi = HashMap::from([("RSSI", Value::from(-60i16))]);

        let data = changed(DEVICE, DEVICE_INTERFACE, device1(&proximity(4)));
        assert!(
            matches!(heard(&data), Some(Heard::Data(Some(a))) if a.data.status == 4),
            "its data"
        );
        let other = changed(DEVICE, DEVICE_INTERFACE, device1(&[0x10, 0x05]));
        assert!(
            matches!(heard(&other), Some(Heard::Data(None))),
            "data of another kind"
        );
        let again = changed(DEVICE, DEVICE_INTERFACE, rssi.clone());
        assert!(matches!(heard(&again), Some(Heard::Again)));
        let announced = added(DEVICE, HashMap::from([(DEVICE_INTERFACE, rssi)]));
        assert!(
            matches!(heard(&announced), Some(Heard::Data(None))),
            "announced with no data"
        );
        let connected = HashMap::from([("Connected", Value::from(true))]);
        assert!(heard(&changed(DEVICE, DEVICE_INTERFACE, connected)).is_none());
    }
}
