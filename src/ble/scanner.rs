//! BLE advertisement scanning via BlueZ D-Bus.
//!
//! Takes the Apple manufacturer data (company ID 0x004C) of each advertisement
//! BlueZ signals on org.bluez.Device1 and parses proximity pairing
//! advertisements out of it. An address heard for the first time becomes a
//! device, announced by the ObjectManager's InterfacesAdded with that first
//! advertisement among its properties; every later one comes as a
//! PropertiesChanged, which carries `ManufacturerData` only when the
//! payload changed, and `RSSI` alone otherwise - those are dropped. Both are
//! read: watching PropertiesChanged alone lost the first advertisement from
//! every address - each pod's, every new one after the lid has been closed,
//! and every one coming back after BlueZ dropped it 30 s into a silence
//! (#86). An address BlueZ already keeps is not announced again, so its
//! unchanged payload is not signaled again until it changes: that happens
//! while another client discovers
//! ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery), #153).
//! Where a monitor reports found a device no signal from bluetoothd has
//! brought since it was last lost, it is read from the device - see below.
//!
//! What makes the adapters hear, per powered adapter, whatever it is called
//! (#51), and decided again whenever the [`AdapterSet`] changes (#50, #189):
//! an advertisement monitor on the pods' proximity pairing data where BlueZ
//! takes one, discovery where it does not - see [`Scanning`] and
//! [app/bluez.md](../../docs/app/bluez.md#listening-for-the-pods). A device the
//! monitor reports found whose payload no signal from bluetoothd has brought
//! since it was last lost - see [`HeardDevices`] - has it read from its properties, as it may come with no
//! InterfacesAdded. The signals come from devices under every
//! adapter, as the subscription takes in every signal bluetoothd sends.
//!
//! Every advertisement that parses goes to the coordinator, which keeps only
//! those it identifies: from an address a stored IRK resolves, decrypted by the
//! same pair's ENC_KEY. The keys are what say which pair sent one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::{Stream, StreamExt};
use tokio::sync::watch;
use tokio::time::Instant;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream, interface, proxy};

use super::parser::{ProximityData, parse_proximity_data};
use crate::bluez::{self, AdapterSet, BLUEZ_SERVICE, Pusher, Queue, coalesce, drain};

/// The key of Apple's entry in a device's `ManufacturerData`.
pub(crate) const APPLE_COMPANY_ID: u16 = 0x004C;

const DEVICE_INTERFACE: &str = "org.bluez.Device1";

/// How long [`ScanControl::pause_discovery`] pauses the app's discovery.
pub const DISCOVERY_PAUSE: Duration = Duration::from_secs(20);

/// Manufacturer specific data, in the Core spec's assigned numbers.
const AD_MANUFACTURER_DATA: u8 = 0xFF;

/// What the monitor matches at the start of manufacturer data: Apple's
/// company ID, little-endian, then the proximity pairing type
/// ([protocol/ble/proximity-pairing.md](../../docs/protocol/ble/proximity-pairing.md)).
pub(crate) const PROXIMITY_PATTERN: [u8; 3] = [0x4C, 0x00, 0x07];

/// Where the scanner's advertisement monitors go, one root per registration
/// below it - see [`monitor_root`].
fn monitor_path() -> String {
    format!("{}/monitor", crate::APP_PATH)
}

/// The root registered with the adapter at `adapter` under the
/// [`bluez::AdapterState::registration`] `token`, named after both: a monitor
/// replaced - its adapter unplugged and plugged in again, say - is at another
/// root than the one that replaced it, so its late `Release` cannot pass for
/// the new one's.
pub(crate) fn monitor_root(adapter: &str, token: u64) -> Result<OwnedObjectPath> {
    let name = adapter
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .with_context(|| format!("{adapter} names no adapter"))?;
    OwnedObjectPath::try_from(format!("{}/{name}_{token}", monitor_path()))
        .with_context(|| format!("no monitor root can be named after {adapter}"))
}

/// The monitor under a root: there is one, for the proximity pairing data.
pub(crate) fn monitor_object(root: &str) -> String {
    format!("{root}/proximity")
}

/// On every adapter's object.
#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
}

/// On an adapter's object while bluetoothd runs with its experimental
/// interfaces on
/// ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)).
#[proxy(
    interface = "org.bluez.AdvertisementMonitorManager1",
    default_service = "org.bluez"
)]
trait AdvertisementMonitorManager1 {
    fn register_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// One parsed advertisement plus the (randomized) MAC it arrived from.
#[derive(Debug, Clone)]
pub struct Advertisement {
    pub data: ProximityData,
    /// The randomized BLE MAC, not the pair's real one.
    pub ble_mac: Address,
}

/// How the adapters listen, for the rest of the app - from
/// [`ScanControl::state`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanState {
    /// Whether a powered adapter has no advertisement monitor, and so has to
    /// discover - also while the discovery is paused, or its starts fail.
    /// While that discovery runs, a device that reconnects by directed
    /// advertising does not connect
    /// ([app/bluez.md](../../docs/app/bluez.md#listening-for-the-pods)).
    pub fallback: bool,
    /// Until when the app's own discovery is paused, if it is.
    pub paused_until: Option<Instant>,
}

/// What the rest of the app reads of the scanning, and the pause it asks
/// for. Made once and handed to every [`Scanner::scan`], so that it outlasts
/// a scan that starts over. Clones share it.
#[derive(Debug, Clone)]
pub struct ScanControl {
    /// Until when discovery is to be paused, as last asked.
    pause: watch::Sender<Option<Instant>>,
    state: watch::Sender<ScanState>,
}

impl Default for ScanControl {
    fn default() -> Self {
        Self {
            pause: watch::Sender::new(None),
            state: watch::Sender::new(ScanState::default()),
        }
    }
}

impl ScanControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// How the adapters listen now, kept as it changes: latest-wins, as a
    /// watch is.
    pub fn state(&self) -> watch::Receiver<ScanState> {
        self.state.subscribe()
    }

    /// Stops the app's discovery on every adapter for [`DISCOVERY_PAUSE`],
    /// and starts it again after that. Asked again meanwhile, the pause runs
    /// for that long from then. Nothing is heard over discovery meanwhile; an
    /// adapter with a monitor goes on hearing.
    pub fn pause_discovery(&self) {
        self.pause_discovery_for(DISCOVERY_PAUSE);
    }

    /// [`ScanControl::pause_discovery`], for `period`.
    pub fn pause_discovery_for(&self, period: Duration) {
        self.pause.send_replace(Some(Instant::now() + period));
    }
}

pub struct Scanner {
    conn: Connection,
    /// Where the monitors served on `conn` tell what BlueZ calls them for,
    /// and where that is read: `None` on a connection built plain, which
    /// serves none.
    monitors: Option<(Pusher<MonitorEvents>, MonitorStream)>,
}

/// What BlueZ called the scanner's monitors for, as it is taken.
type MonitorStream = Pin<Box<dyn Stream<Item = MonitorEvent> + Send + Sync>>;

impl Scanner {
    /// A scanner on the system bus, where BlueZ is.
    pub async fn new() -> Result<Self> {
        Self::connect(zbus::connection::Builder::system()?)
            .await
            .context("failed to connect to system bus")
    }

    /// A scanner on a connection built from `builder` - under the test
    /// harness, to a fake BlueZ on a private bus (#72) - that can serve
    /// advertisement monitors: built with their namespace served, so that it
    /// answers bluetoothd's read of a monitor root from the first message
    /// ([app/bluez.md](../../docs/app/bluez.md#connections-that-serve-objects),
    /// #125).
    pub async fn connect(builder: zbus::connection::Builder<'_>) -> Result<Self> {
        let conn = builder
            .serve_at(monitor_path(), zbus::fdo::ObjectManager)?
            .build()
            .await?;
        let (events, stream) = coalesce::<MonitorEvents>();
        Ok(Self {
            conn,
            monitors: Some((events, Box::pin(stream))),
        })
    }

    /// A scanner on `conn`, for a probe or a test that starts discovery
    /// itself ([`Scanner::start_discovery`]). Built plain, `conn` serves no
    /// monitor, so [`Scanner::scan`] listens by discovery on every adapter.
    pub fn with_connection(conn: Connection) -> Self {
        Self {
            conn,
            monitors: None,
        }
    }

    /// Starts discovery on every powered adapter BlueZ has. One that fails is
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
    /// first power cycle. The app scans with [`Scanner::scan`], which keeps a
    /// monitor or discovery going on every adapter.
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
            match self.start_discovery_on(BLUEZ_SERVICE, &adapter.path).await {
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

    /// Listens on every powered adapter, and keeps it going on each as
    /// adapters change - see [`Scanning`]. Back once each adapter powered
    /// now has a monitor or discovery asked for; there may be none.
    /// `control` hears how they listen, and asks for discovery to pause.
    ///
    /// Subscribed to advertisements first, then to the adapters, so that
    /// nothing falls in between - see [`Scanner::start_discovery`].
    ///
    /// Fails only when either subscription does.
    pub async fn scan(mut self, control: ScanControl) -> Result<Scanning> {
        let heard = Arc::new(Mutex::new(HeardDevices::default()));
        let adverts = Box::pin(self.listen(Some(heard.clone())).await?);
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
        let (monitors, monitor_events) = match self.monitors.take() {
            Some((pusher, stream)) => (Some(pusher), Some(stream)),
            None => (None, None),
        };
        let mut scanning = Scanning {
            conn: self.conn.clone(),
            scanner: self,
            monitors,
            adverts,
            monitor_events,
            adapters,
            pause: control.pause.subscribe(),
            control,
            scans: HashMap::new(),
            roots: HashMap::new(),
            heard,
            bluez_gone: false,
            paused_until: None,
        };
        scanning.update().await;
        Ok(scanning)
    }

    /// The adapter at `adapter`, asked of `bluez`: from [`Scanning`], the
    /// unique name of the bluetoothd the adapters were read from, which no
    /// call starts again
    /// ([app/bluez.md](../../docs/app/bluez.md#never-starting-bluetoothd));
    /// from [`Scanner::start_discovery`], for a probe or a test, `org.bluez`.
    async fn adapter<'a>(&self, bluez: &'a str, adapter: &'a str) -> Result<Adapter1Proxy<'a>> {
        Ok(Adapter1Proxy::builder(&self.conn)
            .destination(bluez)?
            .path(adapter)?
            .build()
            .await?)
    }

    /// Sets an LE-only discovery filter on the adapter at `adapter`, and
    /// starts scanning there - asked of `bluez`, see [`Scanner::adapter`].
    pub(crate) async fn start_discovery_on(&self, bluez: &str, adapter: &str) -> Result<()> {
        let adapter = self.adapter(bluez, adapter).await?;

        let mut filter = HashMap::new();
        filter.insert("Transport", Value::from("le"));
        adapter
            .set_discovery_filter(filter)
            .await
            .context("failed to set discovery filter")?;

        adapter
            .start_discovery()
            .await
            .context("failed to start discovery")?;
        Ok(())
    }

    /// Stops this connection's discovery on the adapter at `adapter`, asked
    /// of `bluez`. BlueZ refuses it when the connection has none there.
    async fn stop_discovery_on(&self, bluez: &str, adapter: &str) -> Result<()> {
        self.adapter(bluez, adapter)
            .await?
            .stop_discovery()
            .await
            .context("failed to stop discovery")?;
        Ok(())
    }

    /// Serves a monitor for the adapter at `adapter` under the registration
    /// `token`, at a root of its own - see [`monitor_root`] - and registers
    /// that root with the adapter's `AdvertisementMonitorManager1`, asked of
    /// `bluez` as [`Scanner::adapter`] is. What BlueZ calls it for goes to
    /// `events`. Served already, as when a failed registration is tried
    /// again, it is kept.
    async fn register_monitor_on(
        &self,
        bluez: &str,
        adapter: &str,
        token: u64,
        events: &Pusher<MonitorEvents>,
    ) -> Result<()> {
        let root = monitor_root(adapter, token)?;
        let server = self.conn.object_server();
        // The root's ObjectManager first, with nothing below it, so that it
        // signals nothing as it comes and the monitor's InterfacesAdded comes
        // from it
        // ([protocol/linux/zbus.md](../../docs/protocol/linux/zbus.md#which-objectmanager-signals)).
        server.at(&root, zbus::fdo::ObjectManager).await?;
        let monitor = ProximityMonitor {
            adapter: adapter.to_string(),
            token,
            events: events.clone(),
        };
        server.at(monitor_object(root.as_str()), monitor).await?;
        AdvertisementMonitorManager1Proxy::builder(&self.conn)
            .destination(bluez)?
            .path(adapter)?
            .build()
            .await?
            .register_monitor(&root)
            .await
            .context("failed to register the advertisement monitor")?;
        Ok(())
    }

    /// Takes the monitor at `root` off the connection, once the registration
    /// it was for is gone.
    async fn drop_monitor(&self, root: &str) {
        let server = self.conn.object_server();
        let _ = server
            .remove::<ProximityMonitor, _>(monitor_object(root))
            .await;
        let _ = server.remove::<zbus::fdo::ObjectManager, _>(root).await;
    }

    /// Yields advertisements as they arrive, as a stream.
    ///
    /// The stream may be read at the caller's pace, with calls to BlueZ on the
    /// scanner's connection in between - [`Scanning`] makes them meanwhile, as
    /// it registers monitors, starts and stops discovery, and reads the
    /// devices a monitor reports found that no signal from bluetoothd has brought since they
    /// were last lost. A task of
    /// its own drains the subscription as the signals arrive, as [`crate::bluez::drain`] does: left in zbus's
    /// queue, 64 advertisements would stop zbus reading the connection, and
    /// the reply to such a call would never come. What waits for the reader
    /// is the latest advertisement from each address - see [`Latest`]. The
    /// task ends with the connection, or when the stream is dropped.
    pub async fn advertisements(
        &self,
    ) -> Result<impl futures_util::Stream<Item = Advertisement> + use<>> {
        self.listen(None).await
    }

    /// [`Scanner::advertisements`], recording in `heard`, if given, every
    /// device whose Apple data a signal carried, with the signal's sender,
    /// as the signal is drained.
    async fn listen(
        &self,
        heard: Option<Arc<Mutex<HeardDevices>>>,
    ) -> Result<impl futures_util::Stream<Item = Advertisement> + use<>> {
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

        let adverts = signals.filter_map(move |msg| {
            let heard = heard.clone();
            async move {
                let msg = msg.ok()?;
                let (path, properties) = device_properties(&msg)?;
                if let Some(heard) = &heard
                    && apple_manufacturer_data(&properties).is_some()
                    && let Some(sender) = msg.header().sender()
                {
                    lock(heard).insert(&path, sender.as_str());
                }
                advertisement(&path, &properties)
            }
        });
        Ok(drain::<Latest, _>(adverts))
    }
}

/// The advertisement among a device's `properties`, the device at `path`, if
/// it carries proximity pairing data that parses.
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

/// The advertisement monitor served for one adapter
/// (`org.bluez.AdvertisementMonitor1`, BlueZ 5.87's
/// `doc/org.bluez.AdvertisementMonitor.rst`): `or_patterns` on
/// [`PROXIMITY_PATTERN`], `RSSISamplingPeriod` 0, and no RSSI threshold, so
/// bluetoothd puts its defaults in their place
/// ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)).
///
/// What BlueZ calls it for goes to [`Scanning`] with the caller's unique
/// name, coalesced, never waiting for the reader.
struct ProximityMonitor {
    /// The adapter it is registered with, by path.
    adapter: String,
    /// The [`bluez::AdapterState::registration`] it was registered under.
    token: u64,
    events: Pusher<MonitorEvents>,
}

impl ProximityMonitor {
    fn tell(&self, header: &Header<'_>, kind: MonitorKind) {
        let Some(sender) = header.sender() else {
            return;
        };
        self.events.push(MonitorEvent {
            sender: sender.to_string(),
            kind,
        });
    }
}

// Not spawned, so calls are taken in the order they came:
// [app/bluez.md](../../docs/app/bluez.md#listening-for-the-pods).
#[interface(name = "org.bluez.AdvertisementMonitor1", spawn = false)]
impl ProximityMonitor {
    fn release(&self, #[zbus(header)] header: Header<'_>) {
        let released = MonitorKind::Released {
            adapter: self.adapter.clone(),
            token: self.token,
        };
        self.tell(&header, released);
    }

    fn activate(&self, #[zbus(header)] header: Header<'_>) {
        self.tell(&header, MonitorKind::Activated(self.adapter.clone()));
    }

    fn device_found(&self, device: OwnedObjectPath, #[zbus(header)] header: Header<'_>) {
        self.tell(&header, MonitorKind::Found(device.to_string()));
    }

    fn device_lost(&self, device: OwnedObjectPath, #[zbus(header)] header: Header<'_>) {
        self.tell(&header, MonitorKind::Lost(device.to_string()));
    }

    #[zbus(property, name = "Type")]
    fn monitor_type(&self) -> String {
        "or_patterns".into()
    }

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

/// A call to one of the scanner's monitors, from the unique name `sender`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MonitorEvent {
    sender: String,
    kind: MonitorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MonitorKind {
    /// The monitor of the adapter at that path is active.
    Activated(String),
    /// The monitor of the adapter at `adapter`, registered under `token`,
    /// was released: it hears nothing more.
    Released { adapter: String, token: u64 },
    /// The device at that path matched.
    Found(String),
    /// The device at that path matched no more.
    Lost(String),
}

/// How many devices [`HeardDevices`] keeps, the oldest forgotten first.
pub const HEARD_DEVICES: usize = 4096;

/// The devices whose payload the scanner's subscription has brought since it
/// began, less those a monitor has since reported lost, by path, with the
/// unique name of the sender that brought each last; at most
/// [`HEARD_DEVICES`] of them. bluetoothd signals every payload change of a
/// device on that subscription
/// ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)),
/// so the payload of one it brought has come already, and a monitor's
/// `DeviceFound` for it needs no read: a flood of new addresses, each found,
/// would otherwise put a call to bluetoothd between every advertisement the
/// reader takes ([app/bluez.md](../../docs/app/bluez.md#listening-for-the-pods)).
/// A device counts as heard only from the bluetoothd the adapters were read
/// from, as any signal does
/// ([app/bluez.md](../../docs/app/bluez.md#which-signals-count)): a forged one
/// costs a read, never a payload. So does a device forgotten.
#[derive(Debug, Default)]
struct HeardDevices {
    order: VecDeque<String>,
    /// Who brought each device in `order` last, by path.
    senders: HashMap<String, String>,
}

impl HeardDevices {
    fn insert(&mut self, path: &str, sender: &str) {
        if let Some(last) = self.senders.get_mut(path) {
            *last = sender.to_string();
            return;
        }
        if self.order.len() == HEARD_DEVICES
            && let Some(oldest) = self.order.pop_front()
        {
            self.senders.remove(&oldest);
        }
        self.senders.insert(path.to_string(), sender.to_string());
        self.order.push_back(path.to_string());
    }

    /// Whether `sender` brought the device at `path` last.
    fn heard_from(&self, path: &str, sender: &str) -> bool {
        self.senders.get(path).is_some_and(|last| last == sender)
    }

    fn remove(&mut self, path: &str) {
        if self.senders.remove(path).is_some() {
            self.order.retain(|heard| heard != path);
        }
    }
}

/// Never held across an await, and neither method panics halfway, so a
/// poisoned lock holds a whole set.
fn lock(heard: &Mutex<HeardDevices>) -> std::sync::MutexGuard<'_, HeardDevices> {
    heard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The monitor calls waiting for [`Scanning`]: each in the order it first
/// came since it was last taken, and the same call again from the same
/// sender only once - but for a device lost, which takes back the device's
/// found waiting before it, so that one found again comes after the loss.
///
/// Anything in range can advertise Apple's proximity pairing data, from as
/// many addresses as it likes, and bluetoothd reports each device found.
/// Queued one per call, they would pile up without bound behind a reader that
/// cannot keep up. A device found twice is read once, as its properties say
/// what both calls would; a release, once, is all there is to tell. A device
/// found and then lost before the reader took either is not read: the monitor
/// heard it no more meanwhile. At most a loss and a find wait per device.
#[derive(Debug, Default)]
pub(crate) struct MonitorEvents {
    order: VecDeque<MonitorEvent>,
    waiting: HashSet<MonitorEvent>,
}

impl Queue for MonitorEvents {
    type Item = MonitorEvent;
    type Out = MonitorEvent;

    fn push(&mut self, event: MonitorEvent) {
        if let MonitorKind::Lost(device) = &event.kind {
            let found = MonitorEvent {
                sender: event.sender.clone(),
                kind: MonitorKind::Found(device.clone()),
            };
            if self.waiting.remove(&found) {
                self.order.retain(|waiting| *waiting != found);
            }
        }
        if self.waiting.insert(event.clone()) {
            self.order.push_back(event);
        }
    }

    fn pop(&mut self) -> Option<MonitorEvent> {
        let event = self.order.pop_front()?;
        self.waiting.remove(&event);
        Some(event)
    }
}

/// Listening on every powered adapter, from [`Scanner::scan`], for as long
/// as the scanner's connection lasts (#50, #189).
///
/// Each adapter gets an advertisement monitor first. Where BlueZ has none to
/// offer - `UnknownMethod`, its experimental interfaces off - or calls the monitor's `Release`, the adapter listens by
/// discovery instead. A registration that fails otherwise is tried again
/// after [`bluez::backoff`], with discovery meanwhile. Discovery is stopped
/// where a monitor is taken, and while [`ScanControl`] has it paused. Why:
/// [app/bluez.md](../../docs/app/bluez.md#listening-for-the-pods).
///
/// What a client has on an adapter is BlueZ's to end: an adapter powering
/// off - the desktop's Bluetooth switch, rfkill, a suspend - ends its
/// discovery there, and one unplugged, or bluetoothd restarting, ends that
/// and the monitor. So each is set up again whenever [`AdapterSet`] says what
/// was set up there is gone: discovery on an adapter that powers on, both on
/// one plugged in or there after bluetoothd restarted. A refused discovery
/// start is tried again after a while, [`bluez::backoff`] longer each time in
/// a row. The advertisement stream is the same throughout: subscribed once,
/// ahead of every start, so that none of them leaves a gap.
///
/// Its own connection's discovery and monitors: bluetoothd keeps both by
/// client.
pub struct Scanning {
    scanner: Scanner,
    conn: Connection,
    /// Where the monitors tell what BlueZ calls them for: `None` when the
    /// scanner's connection serves none.
    monitors: Option<Pusher<MonitorEvents>>,
    adverts: Pin<Box<dyn Stream<Item = Advertisement> + Send + Sync>>,
    monitor_events: Option<MonitorStream>,
    adapters: watch::Receiver<AdapterSet>,
    control: ScanControl,
    pause: watch::Receiver<Option<Instant>>,
    /// How each adapter there is listens, by path.
    scans: HashMap<String, AdapterScan>,
    /// The monitor root served for each adapter's latest registration, by
    /// path.
    roots: HashMap<String, OwnedObjectPath>,
    /// The devices whose payload a signal has brought - see
    /// [`HeardDevices`].
    heard: Arc<Mutex<HeardDevices>>,
    /// Whether BlueZ was not on the bus at the last update.
    bluez_gone: bool,
    /// Until when discovery was paused at the last update, if it was.
    paused_until: Option<Instant>,
}

/// How one adapter listens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AdapterScan {
    monitor: Monitor,
    discovery: Discovery,
}

/// Where the advertisement monitor stands on one adapter. Each holds the
/// [`bluez::AdapterState::registration`] it was decided under, and stands
/// while that does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Monitor {
    /// Not asked for.
    #[default]
    None,
    /// Registered.
    Taken(u64),
    /// Not to be had: BlueZ offers none, the connection serves none, or
    /// BlueZ released it. The adapter listens by discovery.
    Refused(u64),
    /// Refused otherwise, `failures` times in a row, and to be asked for
    /// again at `retry`.
    Failed {
        token: u64,
        failures: u32,
        retry: Instant,
    },
}

/// Where discovery stands on one adapter - see
/// [`bluez::AdapterState::discovery`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Discovery {
    /// The adapter is off.
    #[default]
    Off,
    /// Not wanted: a monitor listens, or discovery is paused.
    Idle,
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
    /// The next advertisement from any adapter. Meanwhile monitors are
    /// registered and discovery started or stopped wherever it has to be.
    /// `None` once the scanner's connection's signals have ended: nothing
    /// more is heard on it.
    pub async fn next(&mut self) -> Option<Advertisement> {
        loop {
            let retry = self.next_retry();
            tokio::select! {
                advert = self.adverts.next() => return advert,
                changed = self.adapters.changed() => {
                    changed.ok()?;
                    self.update().await;
                }
                Some(event) = next_event(&mut self.monitor_events) => {
                    if let Some(advert) = self.take(event).await {
                        return Some(advert);
                    }
                }
                Ok(()) = self.pause.changed() => self.update().await,
                () = until(retry) => self.update().await,
            }
        }
    }

    /// The earliest of the times something is to be tried again, and the
    /// end of a pause.
    fn next_retry(&self) -> Option<Instant> {
        let retries = self.scans.values().flat_map(|scan| {
            let monitor = match scan.monitor {
                Monitor::Failed { retry, .. } => Some(retry),
                _ => None,
            };
            let discovery = match scan.discovery {
                Discovery::Failed { retry, .. } => Some(retry),
                _ => None,
            };
            [monitor, discovery]
        });
        retries.chain([self.paused_until]).flatten().min()
    }

    /// Answers a call to a monitor: only one from the bluetoothd the
    /// adapters were read from counts. A device found that no signal has
    /// brought is read from BlueZ; a device lost is forgotten as heard, so
    /// that it is read when found again.
    async fn take(&mut self, event: MonitorEvent) -> Option<Advertisement> {
        let owner = self.adapters.borrow().owner().map(str::to_string);
        if owner.as_deref() != Some(event.sender.as_str()) {
            tracing::debug!(
                "BLE advertisement monitor call from {} dropped: not from BlueZ",
                event.sender
            );
            return None;
        }
        match event.kind {
            MonitorKind::Activated(adapter) => {
                tracing::debug!("BLE advertisement monitor active on {adapter}");
                None
            }
            MonitorKind::Released { adapter, token } => {
                let registration = self.adapters.borrow().get(&adapter).map(|a| a.registration);
                if let Some(scan) = self.scans.get_mut(&adapter)
                    && releases(registration, scan.monitor, token)
                {
                    tracing::warn!(
                        "BlueZ released the BLE advertisement monitor on {adapter}; \
                         discovery there instead while it is powered"
                    );
                    scan.monitor = Monitor::Refused(token);
                    self.update().await;
                }
                None
            }
            MonitorKind::Lost(device) => {
                tracing::trace!("BLE advertisement monitor lost {device}");
                lock(&self.heard).remove(&device);
                None
            }
            MonitorKind::Found(device) => {
                // Gives the drain a chance to take what came with the call
                // first, so a device announced just before it is usually
                // known as heard; one not yet recorded costs a read.
                tokio::task::yield_now().await;
                if lock(&self.heard).heard_from(&device, &event.sender) {
                    return None;
                }
                self.read_found(&event.sender, &device).await
            }
        }
    }

    /// The advertisement of the device at `device`, as `owner` - the
    /// bluetoothd that reported it found - holds it now: a device found with
    /// no InterfacesAdded, as bluetoothd held it already, has its payload
    /// only there
    /// ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)).
    /// Asked of its unique name, which starts nothing.
    async fn read_found(&self, owner: &str, device: &str) -> Option<Advertisement> {
        if !device.starts_with("/org/bluez/") {
            return None;
        }
        let read = async {
            let props = zbus::fdo::PropertiesProxy::builder(&self.conn)
                .destination(owner)?
                .path(device)?
                .build()
                .await?;
            let iface = zbus::names::InterfaceName::try_from(DEVICE_INTERFACE)?;
            zbus::Result::Ok(props.get_all(iface).await?)
        };
        match read.await {
            Ok(properties) => advertisement(device, &properties),
            Err(e) => {
                tracing::debug!("failed to read the device found at {device}: {e}");
                None
            }
        }
    }

    /// Registers a monitor, and starts or stops discovery, on every adapter
    /// where it has to be, and forgets the adapters that are gone.
    async fn update(&mut self) {
        // Cloned out: the borrow holds the watch's lock.
        let adapters = self.adapters.borrow_and_update().clone();
        let gone = !adapters.on_bus();
        if gone && !self.bluez_gone {
            tracing::info!("BlueZ is not on the bus: no BLE scanning until it is");
            self.scans.clear();
        }
        self.bluez_gone = gone;
        self.scans.retain(|path, _| {
            let here = adapters.get(path).is_some();
            if !here {
                tracing::info!("{path} is gone, and BLE scanning there with it");
            }
            here
        });
        let gone: Vec<String> = self
            .roots
            .keys()
            .filter(|path| adapters.get(path).is_none())
            .cloned()
            .collect();
        for path in gone {
            if let Some(root) = self.roots.remove(&path) {
                self.scanner.drop_monitor(root.as_str()).await;
            }
        }

        let now = Instant::now();
        let paused_until = (*self.pause.borrow_and_update()).filter(|&until| until > now);
        match (self.paused_until, paused_until) {
            (None, Some(until)) => tracing::info!(
                "BLE discovery paused for {}s",
                until.saturating_duration_since(now).as_secs_f32().round()
            ),
            (Some(_), None) => tracing::info!("BLE discovery pause over"),
            _ => {}
        }
        self.paused_until = paused_until;

        // Every call goes to the bluetoothd the adapters were read from, by
        // its unique name; with none, there is no adapter either.
        let owner = adapters.owner().unwrap_or_default();
        for (path, adapter) in adapters.iter() {
            let mut scan = self.scans.get(path).copied().unwrap_or_default();
            let Some(token) = adapter.discovery else {
                match scan.discovery {
                    Discovery::Off => {}
                    Discovery::On(_) => {
                        tracing::info!("BLE discovery on {path} ended: it powered off");
                    }
                    _ => tracing::info!("not scanning on {path}: it is powered off"),
                }
                scan.discovery = Discovery::Off;
                // Asked for again once the adapter is on, from the first
                // wait.
                if matches!(scan.monitor, Monitor::Failed { .. }) {
                    scan.monitor = Monitor::None;
                }
                self.scans.insert(path.to_string(), scan);
                continue;
            };
            scan.monitor = self
                .decide_monitor(owner, path, adapter.registration, scan.monitor, now)
                .await;
            let listens = scan.monitor == Monitor::Taken(adapter.registration);
            scan.discovery = if listens {
                let reason = "an advertisement monitor listens there";
                self.idle(owner, path, scan.discovery, reason).await
            } else if paused_until.is_some() {
                self.idle(owner, path, scan.discovery, "paused").await
            } else {
                self.discover(owner, path, token, scan.discovery, now).await
            };
            self.scans.insert(path.to_string(), scan);
        }

        let fallback = adapters.iter().any(|(path, adapter)| {
            adapter.discovery.is_some()
                && self
                    .scans
                    .get(path)
                    .is_some_and(|scan| scan.monitor != Monitor::Taken(adapter.registration))
        });
        let state = ScanState {
            fallback,
            paused_until,
        };
        self.control.state.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }

    /// The monitor on the adapter at `path` under `registration`, where it
    /// was `was`: asked of `bluez` where nothing stands under that
    /// registration, or a failure's wait is over. The monitor of an earlier
    /// registration there is taken off the connection.
    async fn decide_monitor(
        &mut self,
        bluez: &str,
        path: &str,
        registration: u64,
        was: Monitor,
        now: Instant,
    ) -> Monitor {
        let failures = match was {
            Monitor::Taken(token) | Monitor::Refused(token) if token == registration => return was,
            Monitor::Failed {
                token,
                failures,
                retry,
            } if token == registration => {
                if retry > now {
                    return was;
                }
                failures
            }
            _ => 0,
        };
        let Some(events) = self.monitors.clone() else {
            tracing::info!("BLE discovery on {path}: this connection serves no monitor");
            return Monitor::Refused(registration);
        };
        if let Ok(root) = monitor_root(path, registration)
            && let Some(old) = self.roots.insert(path.to_string(), root.clone())
            && old != root
        {
            self.scanner.drop_monitor(old.as_str()).await;
        }
        let registered = self
            .scanner
            .register_monitor_on(bluez, path, registration, &events)
            .await;
        match registered {
            Ok(()) => {
                tracing::info!("BLE advertisement monitor registered on {path}");
                Monitor::Taken(registration)
            }
            // bluetoothd keys a registration by caller and root
            // ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)),
            // and the root is this registration's: this connection's stands.
            Err(e) if is_bluez_error(&e, "AlreadyExists") => {
                tracing::debug!("BLE advertisement monitor on {path} registered already");
                Monitor::Taken(registration)
            }
            Err(e) if is_unknown_method(&e) => {
                tracing::info!(
                    "BlueZ offers no advertisement monitor on {path}, as its experimental \
                     interfaces are off; BLE discovery there instead"
                );
                Monitor::Refused(registration)
            }
            Err(e) => {
                let failures = failures + 1;
                let wait = bluez::backoff(failures);
                let meanwhile = if self.paused_until.is_some() {
                    "BLE discovery paused meanwhile"
                } else {
                    "with BLE discovery there meanwhile"
                };
                tracing::warn!(
                    "failed to register a BLE advertisement monitor on {path}: {e:#}; \
                     trying again in {}s, {meanwhile}",
                    wait.as_secs()
                );
                Monitor::Failed {
                    token: registration,
                    failures,
                    retry: now + wait,
                }
            }
        }
    }

    /// Discovery on the adapter at `path` stopped, if it was on, for
    /// `reason`, asked of `bluez`.
    async fn idle(&self, bluez: &str, path: &str, was: Discovery, reason: &str) -> Discovery {
        if let Discovery::On(_) = was {
            match self.scanner.stop_discovery_on(bluez, path).await {
                Ok(()) => tracing::info!("BLE discovery stopped on {path}: {reason}"),
                Err(e) => tracing::debug!("BLE discovery on {path} not stopped: {e:#}"),
            }
        }
        Discovery::Idle
    }

    /// Discovery on the adapter at `path` under `token`, where it was `was`:
    /// started where it has to be, asked of `bluez`.
    async fn discover(
        &self,
        bluez: &str,
        path: &str,
        token: u64,
        was: Discovery,
        now: Instant,
    ) -> Discovery {
        let failures = match was {
            Discovery::On(on) if on == token => return was,
            Discovery::Failed {
                token: failed,
                failures,
                retry,
            } if failed == token => {
                if retry > now {
                    return was;
                }
                failures
            }
            _ => 0,
        };
        let mut started = self.scanner.start_discovery_on(bluez, path).await;
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
            && is_bluez_error(e, "InProgress")
        {
            tracing::debug!(
                "BLE discovery on {path} in progress, or refused as busy; asking for it again"
            );
            let _ = self.scanner.stop_discovery_on(bluez, path).await;
            started = self.scanner.start_discovery_on(bluez, path).await;
        }
        match started {
            Ok(()) => {
                let again = matches!(was, Discovery::On(_));
                let started = if again { "restarted" } else { "started" };
                tracing::info!("BLE discovery {started} on {path}");
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
                    retry: now + wait,
                }
            }
        }
    }
}

impl Drop for Scanning {
    /// Nothing listens by discovery once the scanning is gone.
    fn drop(&mut self) {
        self.control.state.send_if_modified(|state| {
            let changed = state.fallback;
            state.fallback = false;
            changed
        });
    }
}

/// Whether a `Release` of the monitor registered under `token` ends the
/// monitor of an adapter whose registration is now `registration` - `None`
/// once the adapter is gone - and whose monitor stands at `monitor`: only
/// that of the monitor that stands. A replaced monitor's release can still be
/// waiting when the adapters already carry the new registration, before the
/// scanner has registered the new monitor and dropped the old root.
fn releases(registration: Option<u64>, monitor: Monitor, token: u64) -> bool {
    registration == Some(token) && monitor == Monitor::Taken(token)
}

/// Whether BlueZ refused a call with `org.bluez.Error.<name>`: `InProgress`
/// to a discovery start, started already by the caller or refused by a busy
/// kernel - see [`Scanning::discover`] - or `AlreadyExists` to a monitor
/// registered already.
fn is_bluez_error(e: &anyhow::Error, name: &str) -> bool {
    matches!(
        e.downcast_ref::<zbus::Error>(),
        Some(zbus::Error::MethodError(error, _, _))
            if error.as_str().strip_prefix("org.bluez.Error.") == Some(name)
    )
}

/// Whether a call was refused as `UnknownMethod`: a monitor registration
/// where bluetoothd runs without its experimental interfaces
/// ([protocol/linux/bluez.md](../../docs/protocol/linux/bluez.md#discovery-and-directed-advertising)).
fn is_unknown_method(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<zbus::Error>(),
        Some(zbus::Error::MethodError(error, _, _))
            if error.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
    )
}

/// The next call to a monitor, or never when there are no monitors.
async fn next_event(events: &mut Option<MonitorStream>) -> Option<MonitorEvent> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// Returns at `at`, or never.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
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

impl Queue for Latest {
    type Item = Advertisement;
    type Out = Advertisement;

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

/// The path of the device a BlueZ signal is about, and the Device1 properties
/// it carries: all of them in the InterfacesAdded that announces a new
/// device, the changed ones in a PropertiesChanged. `None` for any other
/// signal.
///
/// InterfacesAdded comes from the ObjectManager and names the device in its
/// body; PropertiesChanged comes from the device itself. Either way the device
/// is under `/org/bluez`, where BlueZ keeps its adapters and their devices.
fn device_properties(msg: &Message) -> Option<(String, HashMap<String, OwnedValue>)> {
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
            (iface == DEVICE_INTERFACE).then_some((path, changed))
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
            Some((path.to_string(), interfaces.remove(DEVICE_INTERFACE)?))
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

    fn found(device: &str) -> MonitorEvent {
        MonitorEvent {
            sender: ":1.1".into(),
            kind: MonitorKind::Found(device.into()),
        }
    }

    fn lost(device: &str) -> MonitorEvent {
        MonitorEvent {
            sender: ":1.1".into(),
            kind: MonitorKind::Lost(device.into()),
        }
    }

    /// What the reader takes from `pushed`, all pushed before any is taken.
    fn taken(pushed: &[MonitorEvent]) -> Vec<MonitorEvent> {
        let mut events = MonitorEvents::default();
        for event in pushed {
            events.push(event.clone());
        }
        std::iter::from_fn(|| events.pop()).collect()
    }

    /// A device found, lost and found again before the reader takes any of
    /// it reaches the reader lost, then found: it forgets the device as
    /// heard, then reads it. Found twice, it is found once; found and then
    /// lost, it is lost alone; lost, found and lost again, it is lost once.
    /// Another device's calls keep their place.
    #[test]
    fn a_device_found_again_after_a_loss_comes_after_it() {
        const D: &str = "/org/bluez/hci0/dev_4A_00_00_00_00_01";
        const E: &str = "/org/bluez/hci0/dev_4A_00_00_00_00_02";
        assert_eq!(
            taken(&[found(D), found(E), lost(D), found(D)]),
            [found(E), lost(D), found(D)]
        );
        assert_eq!(taken(&[found(D), found(D)]), [found(D)]);
        assert_eq!(taken(&[found(D), lost(D)]), [lost(D)]);
        assert_eq!(taken(&[lost(D), found(D), lost(D)]), [lost(D)]);
    }

    /// A release ends the monitor only where it is the monitor that stands:
    /// registered under the adapter's registration now, and taken. One from
    /// a replaced registration - waiting while the adapters carry the new
    /// one, the old monitor still recorded as taken - ends nothing, nor does
    /// one for an adapter that is gone, or a monitor not taken.
    #[test]
    fn only_the_monitor_that_stands_is_released() {
        assert!(releases(Some(7), Monitor::Taken(7), 7));
        assert!(!releases(Some(8), Monitor::Taken(7), 7), "replaced");
        assert!(
            !releases(Some(8), Monitor::Taken(8), 7),
            "replaced, the new one taken"
        );
        assert!(!releases(None, Monitor::Taken(7), 7), "the adapter gone");
        assert!(!releases(Some(7), Monitor::Refused(7), 7), "not taken");
    }

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

        let (path, properties) = device_properties(&msg).unwrap();
        assert_eq!(path, DEVICE);
        assert_eq!(apple_manufacturer_data(&properties), Some(vec![0x07, 0x19]));
    }

    #[test]
    fn reads_the_advertisement_a_device_changes_with() {
        let msg = changed(DEVICE, DEVICE_INTERFACE, device1(&[0x07, 0x19]));

        let (path, properties) = device_properties(&msg).unwrap();
        assert_eq!(path, DEVICE);
        assert_eq!(apple_manufacturer_data(&properties), Some(vec![0x07, 0x19]));
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
}
