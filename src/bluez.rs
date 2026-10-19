//! BlueZ integration: its device objects, read and watched by [`Devices`], and
//! the batteries [`BatteryProvider`] exports to it for GNOME Settings. Both go
//! over a connection they are given - the system bus, or under the test
//! harness a fake BlueZ on a private one (#72).
//!
//! zbus ships `fdo::ObjectManager` and derives properties, so none of the usual
//! ObjectManager boilerplate is needed - no GetManagedObjects, no introspection
//! XML, no manual InterfacesAdded / InterfacesRemoved / PropertiesChanged:
//! registering the interface at a child path emits InterfacesAdded on its own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, ready};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream, interface, proxy};

pub const BLUEZ_SERVICE: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const PROVIDER_PATH: &str = "/com/github/mstroecker/linuxpods/battery";
const BATTERY_NAME: &str = "airpods_battery";
const SOURCE: &str = "LinuxPods";

/// The service UUID every AirPods lists in `Device1.UUIDs` - the identifier
/// LibrePods uses too. Unlike the alias it survives a rename, and unlike Apple's
/// vendor ID in `Modalias` it does not also match Apple keyboards and mice.
pub const AIRPODS_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

#[proxy(
    interface = "org.bluez.BatteryProviderManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait BatteryProviderManager1 {
    fn register_battery_provider(&self, provider: &ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_battery_provider(&self, provider: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device1 {
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> zbus::Result<Vec<String>>;
}

/// One battery exposed to BlueZ. Properties are read-only; zbus emits
/// PropertiesChanged when they are mutated through the object server.
pub struct BatteryProvider1 {
    percentage: u8,
    device: OwnedObjectPath,
    source: String,
}

#[interface(name = "org.bluez.BatteryProvider1")]
impl BatteryProvider1 {
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        self.percentage
    }

    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        self.device.clone()
    }

    #[zbus(property)]
    fn source(&self) -> String {
        self.source.clone()
    }
}

/// The registered provider, exporting one battery object per device.
///
/// Which objects are exported, and at what level, is the caller's to keep:
/// `attachment` keeps it with the rest of what it knows about each pair, so
/// the two cannot drift apart.
pub struct BatteryProvider {
    conn: Connection,
}

/// Where one device's battery object lives, under the provider root.
fn battery_path(mac: Address) -> Result<OwnedObjectPath> {
    let path = format!(
        "{PROVIDER_PATH}/{BATTERY_NAME}_{}",
        mac.to_string().replace(':', "_")
    );
    OwnedObjectPath::try_from(path.as_str()).with_context(|| format!("invalid battery path {path}"))
}

/// One change to the exported battery objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatteryAction {
    /// Export a device's object, starting at its first reading.
    Add {
        mac: Address,
        device_path: String,
        percentage: u8,
    },
    Update {
        mac: Address,
        percentage: u8,
    },
    /// Withdraw a device's object: its reading went, or the device did.
    Remove {
        mac: Address,
    },
}

impl BatteryAction {
    pub fn mac(&self) -> Address {
        match self {
            Self::Add { mac, .. } | Self::Update { mac, .. } | Self::Remove { mac } => *mac,
        }
    }
}

impl BatteryProvider {
    /// Serves the ObjectManager on `conn` and registers with whichever BlueZ
    /// answers there.
    pub async fn with_connection(conn: Connection) -> Result<Self> {
        // Serving ObjectManager at the provider root is what makes BlueZ pick up
        // batteries added underneath it.
        conn.object_server()
            .at(PROVIDER_PATH, zbus::fdo::ObjectManager)
            .await
            .context("failed to serve ObjectManager")?;

        let manager = BatteryProviderManager1Proxy::new(&conn)
            .await
            .context("failed to reach BatteryProviderManager1")?;
        let path = ObjectPath::try_from(PROVIDER_PATH)?;
        manager
            .register_battery_provider(&path)
            .await
            .context("failed to register battery provider")?;

        Ok(Self { conn })
    }

    /// Carries out one change to the exported battery objects.
    pub async fn apply(&mut self, action: &BatteryAction) -> Result<()> {
        match action {
            BatteryAction::Add {
                mac,
                device_path,
                percentage,
            } => self.add_battery(*mac, device_path, *percentage).await,
            BatteryAction::Update { mac, percentage } => {
                self.update_percentage(*mac, *percentage).await
            }
            BatteryAction::Remove { mac } => self.remove_battery(*mac).await,
        }
    }

    /// Exports one device's battery object at its first reading. Registering the
    /// interface emits InterfacesAdded.
    async fn add_battery(&mut self, mac: Address, device_path: &str, percentage: u8) -> Result<()> {
        let battery_path = battery_path(mac)?;
        let device = OwnedObjectPath::try_from(device_path.to_string())
            .with_context(|| format!("invalid device path {device_path}"))?;

        self.conn
            .object_server()
            .at(
                &battery_path,
                BatteryProvider1 {
                    percentage,
                    device,
                    source: SOURCE.to_string(),
                },
            )
            .await
            .context("failed to export battery object")?;
        Ok(())
    }

    /// Updates one device's percentage, emitting PropertiesChanged.
    async fn update_percentage(&mut self, mac: Address, percentage: u8) -> Result<()> {
        let iface_ref = self
            .conn
            .object_server()
            .interface::<_, BatteryProvider1>(&battery_path(mac)?)
            .await
            .context("battery interface not found")?;

        let mut iface = iface_ref.get_mut().await;
        iface.percentage = percentage;
        iface
            .percentage_changed(iface_ref.signal_emitter())
            .await
            .context("failed to emit PropertiesChanged")?;
        Ok(())
    }

    /// Removes one device's battery object, emitting InterfacesRemoved. Fails
    /// when there is none to remove.
    async fn remove_battery(&mut self, mac: Address) -> Result<()> {
        self.conn
            .object_server()
            .remove::<BatteryProvider1, _>(&battery_path(mac)?)
            .await
            .context("failed to remove battery object")?;
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Unregisters the provider. BlueZ drops its batteries along with it.
    pub async fn close(&self) -> Result<()> {
        let manager = BatteryProviderManager1Proxy::new(&self.conn).await?;
        let path = ObjectPath::try_from(PROVIDER_PATH)?;
        manager.unregister_battery_provider(&path).await?;
        Ok(())
    }
}

/// BlueZ's device objects: which are connected AirPods, what each is called,
/// and the connection watch.
pub struct Devices {
    conn: Connection,
}

impl Devices {
    /// The devices of whichever BlueZ answers on `conn`.
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Every connected AirPods - see [`is_airpods`].
    pub async fn connected_airpods(&self) -> Result<Vec<String>> {
        let om = zbus::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ_SERVICE)?
            .path("/")?
            .build()
            .await?;

        let objects = om
            .get_managed_objects()
            .await
            .context("failed to get managed objects")?;

        Ok(objects
            .into_iter()
            .filter(|(_, interfaces)| {
                interfaces
                    .get("org.bluez.Device1")
                    .is_some_and(|props| is_airpods(props) && bool_prop(props, "Connected"))
            })
            .map(|(path, _)| path.to_string())
            .collect())
    }

    pub async fn device_address(&self, device_path: &str) -> Result<Address> {
        let proxy = Device1Proxy::builder(&self.conn)
            .path(device_path.to_string())?
            .build()
            .await?;
        let address = proxy
            .address()
            .await
            .context("failed to get device address")?;
        address
            .parse()
            .with_context(|| format!("invalid device address {address:?}"))
    }

    /// Every paired device's BlueZ alias, by MAC.
    ///
    /// The alias is the name the user sees everywhere else on the desktop - their
    /// own rename if they made one, the device's advertised name otherwise - so it
    /// is what the device switcher should show.
    pub async fn device_aliases(&self) -> HashMap<Address, String> {
        match self.read_device_aliases().await {
            Ok(names) => names,
            Err(e) => {
                // Names are cosmetic: without them the switcher falls back to the
                // model or the MAC, so this must never be fatal.
                tracing::debug!("failed to read device aliases: {e:#}");
                HashMap::new()
            }
        }
    }

    async fn read_device_aliases(&self) -> Result<HashMap<Address, String>> {
        let om = zbus::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ_SERVICE)?
            .path("/")?
            .build()
            .await?;

        let objects = om
            .get_managed_objects()
            .await
            .context("failed to get managed objects")?;

        Ok(objects
            .into_values()
            .filter_map(|interfaces| device_name_entry(interfaces.get("org.bluez.Device1")?))
            .collect())
    }

    /// Empty string when the device is gone or has no alias.
    pub async fn device_alias(&self, device_path: &str) -> String {
        match self.device_proxy(device_path).await {
            Some(proxy) => proxy.alias().await.unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Whether the device lists the AirPods service - see [`is_airpods`]. False
    /// when the device is gone.
    pub async fn device_is_airpods(&self, device_path: &str) -> bool {
        match self.device_proxy(device_path).await {
            Some(proxy) => proxy
                .uuids()
                .await
                .is_ok_and(|uuids| has_airpods_uuid(&uuids)),
            None => false,
        }
    }

    /// False when the device is gone.
    pub async fn device_connected(&self, device_path: &str) -> bool {
        match self.device_proxy(device_path).await {
            Some(proxy) => proxy.connected().await.unwrap_or(false),
            None => false,
        }
    }

    async fn device_proxy(&self, device_path: &str) -> Option<Device1Proxy<'static>> {
        Device1Proxy::builder(&self.conn)
            .path(device_path.to_string())
            .ok()?
            .build()
            .await
            .ok()
    }
}

/// A device connecting or disconnecting, or its services marking it as AirPods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionEvent {
    pub device_path: String,
    /// `None` when the services changed, not the connection: a first pairing
    /// connects before service discovery fills in the UUID that marks AirPods, so
    /// the device is not recognisable yet when `Connected` changes.
    pub connected: Option<bool>,
}

impl Devices {
    /// Stream of Connected changes on org.bluez.Device1, and of UUIDs changes
    /// that mark a device as AirPods. `Connected` changes come in the order
    /// they came; services changes after them, one per device - see
    /// [`Pending`].
    ///
    /// Port of the Go WatchForAirPods signal loop. Without this the app only ever
    /// saw devices that were already connected at startup, so plugging the
    /// AirPods in later did nothing.
    ///
    /// The stream may be read at the caller's pace, with calls to BlueZ on the
    /// same connection in between: a task of its own drains the signals as they
    /// arrive. The rule takes in every advertisement while the app scans; left
    /// queued in zbus for the caller, 64 of them - a second's worth in a crowd -
    /// stopped zbus reading the connection until there was room, so a reply the
    /// caller was waiting for never arrived, and attachment hung for good. The
    /// task ends with the connection, or when the stream is dropped. The
    /// services changes it keeps for the caller do not pile up either - see
    /// [`Pending`].
    pub async fn watch_connections(&self) -> Result<impl Stream<Item = ConnectionEvent> + use<>> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace("/org/bluez")?
            .build();

        let stream = MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .context("failed to subscribe to device PropertiesChanged")?;

        let events = stream.filter_map(|msg| async move {
            let msg = msg.ok()?;
            let device_path = msg.header().path()?.to_string();

            let (iface, changed, _invalidated) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            if iface != "org.bluez.Device1" {
                return None;
            }

            let connected = match changed.get("Connected") {
                Some(connected) => Some(bool::try_from(connected.clone()).ok()?),
                // The signal carries the whole new list, so the test is the same
                // as on a full property map. Any other UUIDs change - advertisers
                // in range update theirs while we scan - is dropped here, before
                // it costs a round trip to BlueZ.
                None if is_airpods(&changed) => None,
                None => return None,
            };
            Some(ConnectionEvent {
                device_path,
                connected,
            })
        });

        Ok(drain::<Pending, _>(events))
    }
}

/// What [`drain`] holds for its reader, kept as the subscription needs it:
/// [`Pending`] for the connection watch, the latest advertisement from each
/// address for the scanner. Neither `push` nor `pop` may panic halfway - see
/// [`lock`].
pub(crate) trait Queue: Default + Send + 'static {
    type Item: Send + 'static;
    fn push(&mut self, item: Self::Item);
    fn pop(&mut self) -> Option<Self::Item>;
}

/// Reads `events` as they come, in a task of its own, into a `Q`, from where
/// the returned stream takes them at its reader's pace. The task ends with
/// `events`, or once the stream is dropped; the stream ends once the task has
/// and nothing is pending.
pub(crate) fn drain<Q, S>(events: S) -> impl Stream<Item = Q::Item>
where
    Q: Queue,
    S: Stream<Item = Q::Item> + Send + 'static,
{
    let pending = Arc::new(Mutex::new(Q::default()));
    // Rung after every push. It holds one ring at most, so the task never
    // waits on the reader: a ring waiting already means the reader will look.
    let (bell, mut rung) = mpsc::channel(1);
    let queue = pending.clone();
    tokio::spawn(async move {
        tokio::pin!(events);
        loop {
            tokio::select! {
                () = bell.closed() => return,
                event = events.next() => match event {
                    Some(event) => {
                        lock(&queue).push(event);
                        let _ = bell.try_send(());
                    }
                    None => return,
                },
            }
        }
    });
    futures_util::stream::poll_fn(move |cx| {
        loop {
            if let Some(event) = lock(&pending).pop() {
                return Poll::Ready(Some(event));
            }
            // Every push rings after it, so with the task gone and no ring
            // left, nothing more is coming.
            if ready!(rung.poll_recv(cx)).is_none() {
                return Poll::Ready(None);
            }
        }
    })
}

/// The connection events waiting for the watch's reader: every `Connected`
/// change, in order, and one services change per device at most.
///
/// A services change passes the watch's filter for any device that lists the
/// AirPods service, paired or not. An advertiser can list it and a new service
/// in every advertisement, and bluetoothd signals the device's whole list each
/// time. Queued one per signal, they came faster than attachment could ask
/// BlueZ about them, and a connection waited behind them all.
///
/// A services change carries nothing attachment keeps: it reads `Connected`
/// and `UUIDs` back from BlueZ. So one pending per device loses nothing.
/// Dropping changes once there were many instead could drop the one that
/// attaches a pair after its first pairing.
///
/// `Connected` changes come out first, then services changes, each in the
/// order it came. For one device the order makes no difference, as a services
/// change is answered from BlueZ as it is when the change is taken. Across
/// devices it does: a `Connected` change behind services changes, one per
/// device in a crowd, would wait for all of them.
#[derive(Debug, Default)]
struct Pending {
    /// `Connected` changes, oldest first.
    connections: VecDeque<ConnectionEvent>,
    /// Devices whose services changed, by path, in the order each first did
    /// since it was last taken.
    services: VecDeque<String>,
    /// The paths in `services`.
    queued: HashSet<String>,
}

impl Queue for Pending {
    type Item = ConnectionEvent;

    fn push(&mut self, event: ConnectionEvent) {
        match event.connected {
            Some(_) => self.connections.push_back(event),
            None => {
                if self.queued.insert(event.device_path.clone()) {
                    self.services.push_back(event.device_path);
                }
            }
        }
    }

    fn pop(&mut self) -> Option<ConnectionEvent> {
        if let Some(event) = self.connections.pop_front() {
            return Some(event);
        }
        let device_path = self.services.pop_front()?;
        self.queued.remove(&device_path);
        Some(ConnectionEvent {
            device_path,
            connected: None,
        })
    }
}

/// Neither a [`Queue`]'s `push` nor its `pop` can panic halfway, so a poisoned
/// lock holds a whole queue.
fn lock<Q>(pending: &Mutex<Q>) -> MutexGuard<'_, Q> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a Device1 property map lists the AirPods service, [`AIRPODS_UUID`].
///
/// The Go version went by an alias containing "AirPods", but the alias is the
/// user's to change: a renamed pair silently stayed on BLE, and any device with
/// "AirPods" in its name got an L2CAP connect. The alias is for display only.
pub fn is_airpods(props: &HashMap<String, OwnedValue>) -> bool {
    props
        .get("UUIDs")
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .is_some_and(|uuids| has_airpods_uuid(&uuids))
}

/// Case-insensitive: BlueZ reports UUIDs in lowercase, but the spelling is not
/// part of the UUID.
fn has_airpods_uuid(uuids: &[String]) -> bool {
    uuids
        .iter()
        .any(|uuid| uuid.eq_ignore_ascii_case(AIRPODS_UUID))
}

/// A boolean Device1 property; absent or mistyped reads as false.
fn bool_prop(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(false)
}

/// Pulls the (MAC, alias) pair out of a paired device's Device1 property map.
///
/// Both properties are required: an address with no alias has no name to show, and
/// an alias with no address cannot be matched to a device we track. Unpaired devices
/// are skipped: while we scan, BlueZ keeps an object for every advertiser nearby,
/// mostly random addresses named after themselves, and the map goes into every
/// snapshot - unfiltered, a BLE spam flood would grow it. The switcher lists only
/// devices whose key we hold, and getting a key takes a pairing.
fn device_name_entry(props: &HashMap<String, OwnedValue>) -> Option<(Address, String)> {
    if !bool_prop(props, "Paired") {
        return None;
    }
    let string_prop = |k: &str| {
        props
            .get(k)
            .and_then(|v| String::try_from(v.clone()).ok())
            .filter(|s| !s.is_empty())
    };
    let address = string_prop("Address")?;
    let alias = string_prop("Alias")?;
    match address.parse() {
        Ok(mac) => Some((mac, alias)),
        Err(e) => {
            tracing::debug!("skipping the alias of {address:?}: {e}");
            None
        }
    }
}

pub const fn adapter_path() -> &'static str {
    ADAPTER_PATH
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    /// A Device1 property map, as GetManagedObjects or PropertiesChanged carry it.
    fn device<'a>(
        props: impl IntoIterator<Item = (&'a str, Value<'static>)>,
    ) -> HashMap<String, OwnedValue> {
        props
            .into_iter()
            .map(|(key, value)| (key.to_string(), OwnedValue::try_from(value).unwrap()))
            .collect()
    }

    /// A `UUIDs` property listing `uuids`.
    fn uuids(uuids: &[&str]) -> (&'static str, Value<'static>) {
        let uuids: Vec<String> = uuids.iter().map(|u| u.to_string()).collect();
        ("UUIDs", Value::from(uuids))
    }

    const AUDIO_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
    const HANDS_FREE: &str = "0000111e-0000-1000-8000-00805f9b34fb";

    /// Both properties are needed, and the address is parsed, so its spelling
    /// does not matter.
    #[test]
    fn name_entry_needs_an_address_and_an_alias() {
        let paired = ("Paired", Value::from(true));
        let alias = ("Alias", Value::from("Marcel's AirPods Pro"));
        let address = ("Address", Value::from("aa:bb:cc:dd:ee:ff"));

        assert_eq!(
            device_name_entry(&device([paired.clone(), alias.clone()])),
            None
        );
        assert_eq!(
            device_name_entry(&device([paired.clone(), address.clone()])),
            None
        );
        assert_eq!(
            device_name_entry(&device([paired, alias, address])),
            Some((MAC_A, "Marcel's AirPods Pro".to_string()))
        );
    }

    /// An address that does not parse could never match a device we track.
    #[test]
    fn name_entry_needs_a_valid_address() {
        let props = device([
            ("Paired", Value::from(true)),
            ("Alias", Value::from("Marcel's AirPods Pro")),
            ("Address", Value::from("not-a-mac")),
        ]);
        assert_eq!(device_name_entry(&props), None);
    }

    /// Discovery leaves BlueZ an object for every advertiser in range, its alias
    /// the address itself; only a pairing earns a device a name.
    #[test]
    fn name_entry_needs_a_pairing() {
        let mut props = device([
            ("Address", Value::from("DE:AD:BE:EF:00:01")),
            ("Alias", Value::from("DE-AD-BE-EF-00-01")),
        ]);
        assert_eq!(device_name_entry(&props), None);

        props.extend(device([("Paired", Value::from(false))]));
        assert_eq!(device_name_entry(&props), None);

        props.extend(device([("Paired", Value::from(true))]));
        assert_eq!(
            device_name_entry(&props),
            Some((
                Address::new([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]),
                "DE-AD-BE-EF-00-01".to_string()
            ))
        );
    }

    /// One object per device, and a MAC's colons are not legal in an object path.
    #[test]
    fn battery_paths_are_per_device() {
        assert_eq!(
            battery_path(MAC_A).unwrap().as_str(),
            "/com/github/mstroecker/linuxpods/battery/airpods_battery_AA_BB_CC_DD_EE_FF"
        );
        assert_ne!(battery_path(MAC_A).unwrap(), battery_path(MAC_B).unwrap());
    }

    const MAC_A: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    const MAC_B: Address = Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

    #[test]
    fn identifies_airpods_by_service_uuid() {
        assert!(is_airpods(&device([uuids(&[
            AUDIO_SINK,
            HANDS_FREE,
            AIRPODS_UUID
        ])])));
        // Ordinary headphones list the audio profiles alone.
        assert!(!is_airpods(&device([uuids(&[AUDIO_SINK, HANDS_FREE])])));
        assert!(!is_airpods(&device([uuids(&[])])));
    }

    #[test]
    fn service_uuid_ignores_case() {
        assert!(is_airpods(&device([uuids(&[
            "74EC2172-0BAD-4D01-8F77-997B2BE0722A"
        ])])));
    }

    /// No UUIDs - a device service discovery has not reached yet - is not
    /// AirPods, and neither is one merely named so: the alias plays no part.
    #[test]
    fn no_service_uuid_is_not_airpods() {
        assert!(!is_airpods(&device([])));
        assert!(!is_airpods(&device([(
            "Alias",
            Value::from("Marcel's AirPods Pro")
        )])));
        // Mistyped: a bare string where BlueZ sends a list.
        assert!(!is_airpods(&device([("UUIDs", Value::from(AIRPODS_UUID))])));
    }

    /// A rename leaves the UUIDs alone, so the pair is still recognised.
    #[test]
    fn renamed_airpods_are_still_airpods() {
        assert!(is_airpods(&device([
            ("Alias", Value::from("Kitchen buds")),
            uuids(&[AUDIO_SINK, AIRPODS_UUID]),
        ])));
    }

    const PATH_A: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
    const PATH_B: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";

    fn connection(path: &str, connected: bool) -> ConnectionEvent {
        ConnectionEvent {
            device_path: path.to_string(),
            connected: Some(connected),
        }
    }

    fn services(path: &str) -> ConnectionEvent {
        ConnectionEvent {
            device_path: path.to_string(),
            connected: None,
        }
    }

    fn pending(events: impl IntoIterator<Item = ConnectionEvent>) -> Pending {
        let mut pending = Pending::default();
        for event in events {
            pending.push(event);
        }
        pending
    }

    fn take_all(pending: &mut Pending) -> Vec<ConnectionEvent> {
        std::iter::from_fn(|| pending.pop()).collect()
    }

    /// A device signalling its services over and over is one change while it
    /// waits, and one more once that is taken: the one taken may have been
    /// answered before the device changed again.
    #[test]
    fn a_device_resignalling_its_services_is_one_change() {
        let mut pending = pending(std::iter::repeat_n(services(PATH_A), 1000));
        assert_eq!(take_all(&mut pending), [services(PATH_A)]);

        pending.push(services(PATH_A));
        assert_eq!(take_all(&mut pending), [services(PATH_A)]);
    }

    /// Each device has its services change kept, in the order they first came.
    #[test]
    fn devices_have_their_services_changes_kept_apart() {
        let mut pending = pending([services(PATH_B), services(PATH_A), services(PATH_B)]);
        assert_eq!(take_all(&mut pending), [services(PATH_B), services(PATH_A)]);
    }

    /// No connection among services changes is lost or reordered, and all
    /// come out ahead of them.
    #[test]
    fn connections_among_services_changes_keep_their_order() {
        let mut pending = pending([
            services(PATH_A),
            connection(PATH_A, true),
            services(PATH_B),
            services(PATH_A),
            connection(PATH_B, true),
            services(PATH_B),
            connection(PATH_A, false),
            services(PATH_A),
        ]);
        assert_eq!(
            take_all(&mut pending),
            [
                connection(PATH_A, true),
                connection(PATH_B, true),
                connection(PATH_A, false),
                services(PATH_A),
                services(PATH_B),
            ]
        );
    }

    /// A reader far behind the task gets what is pending, and then the end.
    /// The task takes everything in one go here: the events are all there and
    /// nothing in its loop yields, so on this single-threaded runtime the
    /// reader runs again only once the task has ended.
    #[tokio::test]
    async fn the_stream_gives_out_what_is_pending_and_then_ends() {
        let mut events = vec![connection(PATH_A, true)];
        events.extend(std::iter::repeat_n(services(PATH_B), 1000));
        events.push(connection(PATH_A, false));

        let taken: Vec<_> = drain::<Pending, _>(futures_util::stream::iter(events))
            .collect()
            .await;
        assert_eq!(
            taken,
            [
                connection(PATH_A, true),
                connection(PATH_A, false),
                services(PATH_B),
            ]
        );
    }

    /// Dropping the stream ends the task, which drops the subscription.
    #[tokio::test]
    async fn dropping_the_stream_ends_the_task() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let events = drain::<Pending, _>(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        }));
        drop(events);
        tokio::time::timeout(std::time::Duration::from_secs(5), sender.closed())
            .await
            .expect("the task ended");
    }
}
