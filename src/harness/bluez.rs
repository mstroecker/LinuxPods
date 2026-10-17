//! A fake BlueZ: `org.bluez` on a private bus, for the app's scanner and
//! battery provider to talk to in place of bluetoothd (#72).
//!
//! It serves what `ble::scanner` and `bluez` call and listen to, and answers
//! there as bluetoothd does, errors included:
//!
//! - the ObjectManager at `/`, which `bluez` lists devices from
//! - `/org/bluez/hci0`: `Adapter1`, whose discovery is per client, and
//!   `BatteryProviderManager1`
//! - a `Device1` for each paired pair, at its real address: `Address`,
//!   `Alias`, `Connected`, `Paired` and `UUIDs`, the AirPods service among them
//!   unless [`FakeBluez::set_airpods_service`] took it out
//! - a `Device1` for each random address an advertisement came from, with its
//!   `ManufacturerData`
//! - `Battery1` on a device once a registered provider gives it a level - the
//!   battery GNOME Settings shows
//!
//! Nothing else of bluetoothd is here - no pairing, no `Device1.Connect`, no
//! agent - since the app uses none of it.
//!
//! # Advertisements
//!
//! Signalled as bluetoothd's source has it, until the captures of #71 confirm
//! or correct that:
//!
//! - An address heard for the first time becomes a device object, announced
//!   by `InterfacesAdded` with its properties, `ManufacturerData` among them.
//!   bluetoothd fills the object in before it announces it, so no
//!   `PropertiesChanged` follows for that first advertisement.
//! - Each later advertisement from the address is a `PropertiesChanged` of
//!   `ManufacturerData` on `org.bluez.Device1`.
//! - An address not heard for [`TEMPORARY_TIMEOUT`] goes with
//!   `InterfacesRemoved`.
//!
//! Advertisements are heard only while some client has discovery on, as an
//! adapter hears them only while it scans.
//!
//! # Batteries
//!
//! A provider registers the root of its objects, and the fake reads them from
//! there as bluetoothd does: the ObjectManager's objects at once, then its
//! `InterfacesAdded`, `InterfacesRemoved` and `PropertiesChanged`. A battery
//! takes its device from `Device` when it appears, and is ignored if no such
//! device exists or another provider got there first. A provider that leaves
//! the bus takes its batteries along, as does one whose client stops
//! discovering on leaving.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::StreamExt;
use tokio::task::AbortHandle;
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, interface};

use crate::ble::scanner::APPLE_COMPANY_ID;
use crate::bluez::{AIRPODS_UUID, BLUEZ_SERVICE, adapter_path};

/// How long bluetoothd keeps a device it has only heard advertise once it
/// stops hearing it: `TemporaryTimeout` in its main.conf, 30 seconds unless
/// set otherwise.
pub const TEMPORARY_TIMEOUT: Duration = Duration::from_secs(30);

/// The adapter's address, from the block RFC 7042 sets aside for
/// documentation - as are the scenarios' pairs - so it is nobody's.
const ADAPTER_ADDRESS: &str = "00:00:5E:00:53:00";

/// The services a pair of AirPods lists once paired: audio sink, remote
/// control target and controller, hands-free, PnP information, and the one
/// that marks it as AirPods.
const PAIR_UUIDS: [&str; 6] = [
    "0000110b-0000-1000-8000-00805f9b34fb",
    "0000110c-0000-1000-8000-00805f9b34fb",
    "0000110e-0000-1000-8000-00805f9b34fb",
    "0000111e-0000-1000-8000-00805f9b34fb",
    "00001200-0000-1000-8000-00805f9b34fb",
    AIRPODS_UUID,
];

/// The signal strength every advertisement arrives at. Nothing reads it.
const RSSI: i16 = -60;

const PROVIDER_INTERFACE: &str = "org.bluez.BatteryProvider1";

/// Where bluetoothd puts a device: `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
pub fn device_path(address: Address) -> OwnedObjectPath {
    let path = format!(
        "{}/dev_{}",
        adapter_path(),
        address.to_string().replace(':', "_")
    );
    OwnedObjectPath::try_from(path).expect("a MAC makes a valid path element")
}

/// The address in a device path, if it is one.
fn address_of(path: &str) -> Option<Address> {
    let device = path.strip_prefix(adapter_path())?.strip_prefix("/dev_")?;
    device.replace('_', ":").parse().ok()
}

/// bluetoothd's errors, by the names it gives them.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum BluezError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InvalidArguments(String),
    InProgress(String),
    Failed(String),
    AlreadyExists(String),
    DoesNotExist(String),
}

/// The caller of a method, which bluetoothd keeps discovery and providers by.
fn sender(header: &Header<'_>) -> Result<OwnedUniqueName, BluezError> {
    header
        .sender()
        .map(|name| name.to_owned().into())
        .ok_or_else(|| BluezError::Failed("the call names no sender".into()))
}

/// Checks a discovery filter as bluetoothd does: every key one it knows, with
/// its type, and a transport it knows. What a filter asks for is not applied:
/// every advertisement here is LE and in range.
fn check_filter(filter: &HashMap<String, OwnedValue>) -> Result<(), String> {
    for (key, value) in filter {
        let expected = match key.as_str() {
            "UUIDs" => "as",
            "RSSI" => "n",
            "Pathloss" => "q",
            "Transport" | "Pattern" => "s",
            "DuplicateData" | "Discoverable" => "b",
            _ => return Err(format!("unknown discovery filter {key:?}")),
        };
        let signature = value.value_signature().to_string();
        if signature != expected {
            return Err(format!("{key} is {signature}, not {expected}"));
        }
    }
    if let Some(transport) = filter.get("Transport") {
        let transport = <&str>::try_from(&**transport).map_err(|e| e.to_string())?;
        if !["auto", "bredr", "le"].contains(&transport) {
            return Err(format!("unknown transport {transport:?}"));
        }
    }
    Ok(())
}

/// A fake `org.bluez` on one connection. Clones share it.
#[derive(Clone)]
pub struct FakeBluez {
    conn: Connection,
    state: Arc<Mutex<State>>,
    /// Held while a device's `Battery1` is brought in line with its battery,
    /// so two updates cannot interleave.
    exports: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct State {
    paired: HashSet<Address>,
    /// Random addresses heard advertising, and when each was last heard.
    advertisers: HashMap<Address, Instant>,
    /// The clients that have discovery on.
    discovering: HashSet<OwnedUniqueName>,
    /// Registered providers, by owner and root, with the task reading each.
    providers: HashMap<Provider, AbortHandle>,
    /// The battery each device has from a provider, by device path.
    batteries: HashMap<OwnedObjectPath, Battery>,
}

/// A battery provider: who registered it, and its root path.
type Provider = (OwnedUniqueName, OwnedObjectPath);

struct Battery {
    address: Address,
    provider: Provider,
    /// The provider's object for this battery.
    object: OwnedObjectPath,
    /// `None` until the provider gives a level of at most 100.
    percentage: Option<u8>,
    source: String,
}

impl State {
    /// Whether a device object is at `path`.
    fn has_device(&self, path: &str) -> bool {
        address_of(path)
            .is_some_and(|a| self.paired.contains(&a) || self.advertisers.contains_key(&a))
    }

    /// Removes the batteries of the providers `provider` picks, returning
    /// their device paths.
    fn take_batteries(&mut self, provider: impl Fn(&Provider) -> bool) -> Vec<OwnedObjectPath> {
        let devices: Vec<OwnedObjectPath> = self
            .batteries
            .iter()
            .filter(|(_, b)| provider(&b.provider))
            .map(|(device, _)| device.clone())
            .collect();
        for device in &devices {
            self.batteries.remove(device);
        }
        devices
    }
}

impl FakeBluez {
    /// Serves the adapter and the ObjectManager on `conn`. The name comes
    /// later, from [`FakeBluez::publish`], so that devices can be put in place
    /// before the app can find any of it.
    pub async fn serve(conn: Connection) -> Result<Self> {
        let bluez = Self {
            conn: conn.clone(),
            state: Arc::default(),
            exports: Arc::default(),
        };
        let server = conn.object_server();
        server.at("/", zbus::fdo::ObjectManager).await?;
        let adapter = Adapter1 {
            bluez: bluez.clone(),
        };
        server.at(adapter_path(), adapter).await?;
        let manager = BatteryProviderManager1 {
            bluez: bluez.clone(),
        };
        server.at(adapter_path(), manager).await?;

        // bluetoothd forgets a client's discovery and providers when it
        // leaves the bus.
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut owners = dbus.receive_name_owner_changed().await?;
        let watcher = bluez.clone();
        tokio::spawn(async move {
            while let Some(signal) = owners.next().await {
                let Ok(args) = signal.args() else { continue };
                if let BusName::Unique(name) = args.name()
                    && args.new_owner().is_none()
                {
                    watcher.client_gone(name.to_owned().into()).await;
                }
            }
        });
        Ok(bluez)
    }

    /// Takes the name `org.bluez`, under which the app looks for BlueZ.
    pub async fn publish(&self) -> Result<()> {
        self.conn
            .request_name(BLUEZ_SERVICE)
            .await
            .context("failed to take org.bluez")
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a device paired with the adapter, disconnected.
    pub async fn add_device(&self, address: Address, alias: &str) -> Result<()> {
        anyhow::ensure!(
            self.state().paired.insert(address),
            "{address} is paired already"
        );
        let device = PairedDevice {
            address,
            alias: alias.to_string(),
            connected: false,
            airpods_service: true,
        };
        self.conn
            .object_server()
            .at(device_path(address), device)
            .await?;
        Ok(())
    }

    /// Connects or disconnects a paired device. A change is signalled, as
    /// bluetoothd signals it; setting what is already set signals nothing.
    pub async fn set_connected(&self, address: Address, connected: bool) -> Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, PairedDevice>(device_path(address))
            .await
            .with_context(|| format!("no paired device {address}"))?;
        let mut device = iface.get_mut().await;
        if device.connected != connected {
            device.connected = connected;
            device.connected_changed(iface.signal_emitter()).await?;
        }
        Ok(())
    }

    /// Lists the AirPods service among a paired device's services, or takes it
    /// out, signalling the change as bluetoothd signals services found. Without
    /// it, the device is ordinary headphones to the app; and a pair that
    /// connects before it is listed is one being paired for the first time,
    /// connected before service discovery is done.
    pub async fn set_airpods_service(&self, address: Address, listed: bool) -> Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, PairedDevice>(device_path(address))
            .await
            .with_context(|| format!("no paired device {address}"))?;
        let mut device = iface.get_mut().await;
        if device.airpods_service != listed {
            device.airpods_service = listed;
            // Named by zbus after the D-Bus name, `UUIDs`, capital by capital.
            device.u_u_i_ds_changed(iface.signal_emitter()).await?;
        }
        Ok(())
    }

    /// Whether any client has discovery on.
    pub fn is_discovering(&self) -> bool {
        !self.state().discovering.is_empty()
    }

    /// An advertisement from `address`, `apple_data` the value of its Apple
    /// entry in `ManufacturerData`. Signalled as the module docs describe, and
    /// only while discovering: returns whether it was.
    pub async fn advertise(&self, address: Address, apple_data: &[u8]) -> Result<bool> {
        let heard_before = {
            let mut state = self.state();
            if state.discovering.is_empty() {
                return Ok(false);
            }
            anyhow::ensure!(
                !state.paired.contains(&address),
                "{address} is a paired device's address"
            );
            state.advertisers.insert(address, Instant::now()).is_some()
        };

        let path = device_path(address);
        let server = self.conn.object_server();
        let advertiser = || Advertiser {
            address,
            apple_data: apple_data.to_vec(),
        };
        // `at` announces the object with its properties.
        if !heard_before && server.at(&path, advertiser()).await? {
            return Ok(true);
        }
        match server.interface::<_, Advertiser>(&path).await {
            Ok(iface) => {
                let mut advertiser = iface.get_mut().await;
                advertiser.apple_data = apple_data.to_vec();
                advertiser
                    .manufacturer_data_changed(iface.signal_emitter())
                    .await?;
            }
            // Pruned in between, so it is new again.
            Err(_) => {
                server.at(&path, advertiser()).await?;
            }
        }
        Ok(true)
    }

    /// Removes the devices only heard advertising that have not been heard for
    /// [`TEMPORARY_TIMEOUT`] by `now`.
    pub async fn prune(&self, now: Instant) {
        let stale: Vec<Address> = {
            let mut state = self.state();
            let stale: Vec<Address> = state
                .advertisers
                .iter()
                .filter(|(_, heard)| now.saturating_duration_since(**heard) >= TEMPORARY_TIMEOUT)
                .map(|(address, _)| *address)
                .collect();
            for address in &stale {
                state.advertisers.remove(address);
            }
            stale
        };
        let server = self.conn.object_server();
        for address in stale {
            let _ = server.remove::<Advertiser, _>(device_path(address)).await;
        }
    }

    /// Every device's battery level, as `Battery1` shows it and GNOME Settings
    /// with it.
    pub fn batteries(&self) -> HashMap<Address, u8> {
        self.state()
            .batteries
            .values()
            .filter_map(|b| Some((b.address, b.percentage?)))
            .collect()
    }

    fn register_provider(
        &self,
        owner: OwnedUniqueName,
        root: OwnedObjectPath,
    ) -> Result<(), BluezError> {
        let mut state = self.state();
        let provider = (owner, root);
        if state.providers.contains_key(&provider) {
            return Err(BluezError::AlreadyExists("Already Exists".into()));
        }
        let task = tokio::spawn(self.clone().read_provider(provider.clone()));
        state.providers.insert(provider, task.abort_handle());
        Ok(())
    }

    async fn unregister_provider(&self, provider: Provider) -> Result<(), BluezError> {
        let devices = {
            let mut state = self.state();
            let Some(task) = state.providers.remove(&provider) else {
                return Err(BluezError::DoesNotExist("Does Not Exist".into()));
            };
            task.abort();
            state.take_batteries(|p| *p == provider)
        };
        for device in devices {
            self.sync_battery(&device).await;
        }
        Ok(())
    }

    /// A connection left the bus: its discovery stops, and its providers go.
    async fn client_gone(&self, name: OwnedUniqueName) {
        let (stopped, devices) = {
            let mut state = self.state();
            let stopped = state.discovering.remove(&name) && state.discovering.is_empty();
            state.providers.retain(|(owner, _), task| {
                let gone = *owner == name;
                if gone {
                    task.abort();
                }
                !gone
            });
            (stopped, state.take_batteries(|(owner, _)| *owner == name))
        };
        if stopped {
            let _ = self.signal_discovering().await;
        }
        for device in devices {
            self.sync_battery(&device).await;
        }
    }

    /// Signals `Discovering` from outside an `Adapter1` method.
    async fn signal_discovering(&self) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Adapter1>(adapter_path())
            .await?;
        iface
            .get()
            .await
            .discovering_changed(iface.signal_emitter())
            .await
    }

    /// Reads one provider's batteries until it is unregistered or leaves.
    async fn read_provider(self, provider: Provider) {
        if let Err(e) = self.try_read_provider(&provider).await {
            tracing::warn!(
                "fake BlueZ: stopped reading battery provider {} at {}: {e}",
                provider.0,
                provider.1
            );
        }
    }

    async fn try_read_provider(&self, provider: &Provider) -> zbus::Result<()> {
        let (owner, root) = provider;
        let manager = zbus::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination(owner.as_ref())?
            .path(root.as_ref())?
            .build()
            .await?;
        // Subscribed before the objects are read, so nothing falls in between.
        let mut added = manager.receive_interfaces_added().await?;
        let mut removed = manager.receive_interfaces_removed().await?;
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(owner.as_ref())?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace(root.as_ref())?
            .build();
        let mut changed = MessageStream::for_match_rule(rule, &self.conn, None).await?;

        for (object, interfaces) in manager.get_managed_objects().await? {
            let props = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == PROVIDER_INTERFACE);
            if let Some((_, props)) = props {
                let provided = Provided::read(props.iter().map(|(k, v)| (k.as_str(), &**v)));
                self.battery_added(provider, object, provided).await;
            }
        }

        loop {
            tokio::select! {
                Some(signal) = added.next() => {
                    let args = signal.args()?;
                    let props = args
                        .interfaces_and_properties()
                        .iter()
                        .find(|(name, _)| name.as_str() == PROVIDER_INTERFACE);
                    if let Some((_, props)) = props {
                        let provided = Provided::read(props.iter().map(|(k, v)| (*k, v)));
                        let object = args.object_path().to_owned().into();
                        self.battery_added(provider, object, provided).await;
                    }
                }
                Some(signal) = removed.next() => {
                    let args = signal.args()?;
                    if args.interfaces().iter().any(|name| name.as_str() == PROVIDER_INTERFACE) {
                        self.battery_removed(provider, args.object_path()).await;
                    }
                }
                Some(msg) = changed.next() => {
                    let Ok(msg) = msg else { continue };
                    let Some(object) = msg.header().path().map(|p| p.to_owned()) else {
                        continue;
                    };
                    let Ok((iface, props, _)) = msg
                        .body()
                        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    else {
                        continue;
                    };
                    if iface == PROVIDER_INTERFACE {
                        let provided = Provided::read(props.iter().map(|(k, v)| (k.as_str(), &**v)));
                        self.battery_changed(provider, &object, provided.percentage).await;
                    }
                }
                else => return Ok(()),
            }
        }
    }

    async fn battery_added(
        &self,
        provider: &Provider,
        object: OwnedObjectPath,
        provided: Provided,
    ) {
        let Some(device) = provided.device else {
            return;
        };
        {
            let mut state = self.state();
            if !state.has_device(&device) {
                tracing::warn!(
                    "fake BlueZ: ignoring battery for non-existent device path {device}"
                );
                return;
            }
            if state.batteries.contains_key(&device) {
                tracing::warn!("fake BlueZ: battery {device} is already provided");
                return;
            }
            let battery = Battery {
                address: address_of(&device).expect("has_device parsed it"),
                provider: provider.clone(),
                object,
                percentage: provided.percentage.filter(|p| *p <= 100),
                source: provided.source.unwrap_or_default(),
            };
            state.batteries.insert(device.clone(), battery);
        }
        self.sync_battery(&device).await;
    }

    async fn battery_changed(
        &self,
        provider: &Provider,
        object: &ObjectPath<'_>,
        percentage: Option<u8>,
    ) {
        // bluetoothd takes a level of at most 100 and ignores the rest.
        let Some(percentage) = percentage.filter(|p| *p <= 100) else {
            return;
        };
        let device = {
            let mut state = self.state();
            let Some((device, battery)) = state
                .batteries
                .iter_mut()
                .find(|(_, b)| b.provider == *provider && b.object.as_str() == object.as_str())
            else {
                return;
            };
            battery.percentage = Some(percentage);
            device.clone()
        };
        self.sync_battery(&device).await;
    }

    async fn battery_removed(&self, provider: &Provider, object: &ObjectPath<'_>) {
        let device = {
            let mut state = self.state();
            let device = state
                .batteries
                .iter()
                .find(|(_, b)| b.provider == *provider && b.object.as_str() == object.as_str())
                .map(|(device, _)| device.clone());
            if let Some(device) = &device {
                state.batteries.remove(device);
            }
            device
        };
        if let Some(device) = device {
            self.sync_battery(&device).await;
        }
    }

    /// Brings the device's `Battery1` in line with the battery it has: there
    /// with its level, or gone without one.
    async fn sync_battery(&self, device: &OwnedObjectPath) {
        let _exporting = self.exports.lock().await;
        let wanted = self
            .state()
            .batteries
            .get(device)
            .and_then(|b| Some((b.percentage?, b.source.clone())));
        let server = self.conn.object_server();
        let current = server.interface::<_, Battery1>(device).await.ok();
        let result = match (wanted, current) {
            (Some((percentage, _)), Some(iface)) => {
                let mut battery = iface.get_mut().await;
                if battery.percentage == percentage {
                    Ok(())
                } else {
                    battery.percentage = percentage;
                    battery.percentage_changed(iface.signal_emitter()).await
                }
            }
            (Some((percentage, source)), None) => server
                .at(device, Battery1 { percentage, source })
                .await
                .map(|_| ()),
            (None, Some(_)) => server.remove::<Battery1, _>(device).await.map(|_| ()),
            (None, None) => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!("fake BlueZ: failed to update the battery of {device}: {e}");
        }
    }
}

/// What a provider's battery object says, as far as it says it.
#[derive(Debug, Default)]
struct Provided {
    device: Option<OwnedObjectPath>,
    percentage: Option<u8>,
    source: Option<String>,
}

impl Provided {
    fn read<'a>(props: impl IntoIterator<Item = (&'a str, &'a Value<'a>)>) -> Self {
        let mut provided = Self::default();
        for (name, value) in props {
            match name {
                "Device" => {
                    provided.device = value
                        .downcast_ref::<ObjectPath<'_>>()
                        .ok()
                        .map(|p| p.to_owned().into())
                }
                "Percentage" => provided.percentage = value.downcast_ref::<u8>().ok(),
                "Source" => provided.source = value.downcast_ref::<&str>().ok().map(str::to_string),
                _ => {}
            }
        }
        provided
    }
}

struct Adapter1 {
    bluez: FakeBluez,
}

#[interface(name = "org.bluez.Adapter1")]
impl Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<String, OwnedValue>) -> Result<(), BluezError> {
        check_filter(&filter).map_err(BluezError::InvalidArguments)
    }

    async fn start_discovery(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), BluezError> {
        let client = sender(&header)?;
        let started = {
            let mut state = self.bluez.state();
            let idle = state.discovering.is_empty();
            if !state.discovering.insert(client) {
                return Err(BluezError::InProgress(
                    "Operation already in progress".into(),
                ));
            }
            idle
        };
        if started {
            self.discovering_changed(&emitter).await?;
        }
        Ok(())
    }

    async fn stop_discovery(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), BluezError> {
        let client = sender(&header)?;
        let stopped = {
            let mut state = self.bluez.state();
            if !state.discovering.remove(&client) {
                return Err(BluezError::Failed("No discovery started".into()));
            }
            state.discovering.is_empty()
        };
        if stopped {
            self.discovering_changed(&emitter).await?;
        }
        Ok(())
    }

    #[zbus(property)]
    fn address(&self) -> String {
        ADAPTER_ADDRESS.to_string()
    }

    #[zbus(property)]
    fn address_type(&self) -> String {
        "public".to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        "fakepods".to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.name()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.bluez.is_discovering()
    }
}

struct BatteryProviderManager1 {
    bluez: FakeBluez,
}

#[interface(name = "org.bluez.BatteryProviderManager1")]
impl BatteryProviderManager1 {
    fn register_battery_provider(
        &self,
        provider: OwnedObjectPath,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BluezError> {
        self.bluez.register_provider(sender(&header)?, provider)
    }

    async fn unregister_battery_provider(
        &self,
        provider: OwnedObjectPath,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BluezError> {
        let owner = sender(&header)?;
        self.bluez.unregister_provider((owner, provider)).await
    }
}

/// A pair paired with the adapter, at its real address.
struct PairedDevice {
    address: Address,
    alias: String,
    connected: bool,
    /// Whether `UUIDs` lists the AirPods service.
    airpods_service: bool,
}

#[interface(name = "org.bluez.Device1")]
impl PairedDevice {
    #[zbus(property)]
    fn address(&self) -> String {
        self.address.to_string()
    }

    #[zbus(property)]
    fn address_type(&self) -> String {
        "public".to_string()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.alias.clone()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.alias.clone()
    }

    #[zbus(property)]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(adapter_path()).expect("a valid path")
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn bonded(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.connected
    }

    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        PAIR_UUIDS
            .iter()
            .filter(|uuid| self.airpods_service || **uuid != AIRPODS_UUID)
            .map(|uuid| uuid.to_string())
            .collect()
    }
}

/// A random address heard advertising: what bluetoothd makes of a pair it
/// cannot resolve, or of anything else in range.
struct Advertiser {
    address: Address,
    /// The value of the Apple entry in its manufacturer data.
    apple_data: Vec<u8>,
}

#[interface(name = "org.bluez.Device1")]
impl Advertiser {
    #[zbus(property)]
    fn address(&self) -> String {
        self.address.to_string()
    }

    #[zbus(property)]
    fn address_type(&self) -> String {
        "random".to_string()
    }

    /// With no name heard, bluetoothd shows the address, dashed.
    #[zbus(property)]
    fn alias(&self) -> String {
        self.address.to_string().replace(':', "-")
    }

    #[zbus(property)]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(adapter_path()).expect("a valid path")
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn bonded(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }

    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> i16 {
        RSSI
    }

    #[zbus(property)]
    fn manufacturer_data(&self) -> HashMap<u16, OwnedValue> {
        let data = Value::from(self.apple_data.clone())
            .try_into_owned()
            .expect("bytes carry no file descriptor");
        HashMap::from([(APPLE_COMPANY_ID, data)])
    }
}

/// The battery bluetoothd puts on a device for a provider.
struct Battery1 {
    percentage: u8,
    source: String,
}

#[interface(name = "org.bluez.Battery1")]
impl Battery1 {
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        self.percentage
    }

    #[zbus(property)]
    fn source(&self) -> String {
        self.source.clone()
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tokio::time::timeout;
    use zbus::proxy::CacheProperties;

    use super::*;
    use crate::ble::scanner::Scanner;
    use crate::bluez::{BatteryAction, BatteryProvider, Devices};
    use crate::harness::PrivateBus;
    use crate::harness::pods::{PRO_3, Pods};

    /// Plenty for any of these, and a hang fails the test instead of CI.
    const TIMEOUT: Duration = Duration::from_secs(5);

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const ALIAS: &str = "Fake AirPods";
    /// A random address, as an advertisement comes from.
    const RANDOM: Address = Address::new([0x4A, 0x11, 0x22, 0x33, 0x44, 0x55]);

    /// A fake BlueZ on a bus of its own with one pair paired, under its name.
    async fn served() -> (PrivateBus, FakeBluez) {
        let bus = PrivateBus::start().unwrap();
        let bluez = FakeBluez::serve(bus.connect().await.unwrap())
            .await
            .unwrap();
        bluez.add_device(PAIR, ALIAS).await.unwrap();
        bluez.publish().await.unwrap();
        (bus, bluez)
    }

    /// Waits until `done`, which the fake gets to by itself.
    async fn eventually(done: impl Fn() -> bool) {
        timeout(TIMEOUT, async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out");
    }

    /// The D-Bus error a call failed with.
    fn error_name(result: zbus::Result<zbus::message::Message>) -> String {
        match result {
            Err(zbus::Error::MethodError(name, _, _)) => name.to_string(),
            other => panic!("expected a D-Bus error, got {other:?}"),
        }
    }

    /// A proxy to the fake that reads every property afresh.
    async fn proxy<'a>(conn: &Connection, path: &'a str, iface: &'a str) -> zbus::Proxy<'a> {
        zbus::proxy::Builder::new(conn)
            .destination(BLUEZ_SERVICE)
            .unwrap()
            .path(path)
            .unwrap()
            .interface(iface)
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    /// The Apple entry of the `ManufacturerData` among `props`.
    fn apple_data(props: &HashMap<String, OwnedValue>) -> Vec<u8> {
        let data = <HashMap<u16, OwnedValue>>::try_from(props["ManufacturerData"].clone()).unwrap();
        <Vec<u8>>::try_from(data[&APPLE_COMPANY_ID].clone()).unwrap()
    }

    #[test]
    fn device_paths_name_the_address() {
        let path = device_path(PAIR);
        assert_eq!(path.as_str(), "/org/bluez/hci0/dev_00_00_5E_00_53_01");
        assert_eq!(address_of(&path), Some(PAIR));
        assert_eq!(address_of("/org/bluez/hci0"), None);
        assert_eq!(address_of("/org/bluez/hci1/dev_00_00_5E_00_53_01"), None);
    }

    /// The app's filter passes, and what bluetoothd refuses is refused.
    #[test]
    fn discovery_filters_are_checked() {
        let filter = |entries: Vec<(&str, Value<'static>)>| -> HashMap<String, OwnedValue> {
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.try_into_owned().unwrap()))
                .collect()
        };
        assert!(check_filter(&filter(vec![("Transport", Value::from("le"))])).is_ok());
        assert!(check_filter(&filter(vec![])).is_ok());
        let full = filter(vec![
            ("UUIDs", Value::from(vec![AIRPODS_UUID])),
            ("RSSI", Value::from(-70i16)),
            ("DuplicateData", Value::from(false)),
        ]);
        assert!(check_filter(&full).is_ok());
        for refused in [
            vec![("Transport", Value::from("radio"))],
            vec![("Transport", Value::from(true))],
            vec![("RSSI", Value::from("-70"))],
            vec![("Range", Value::from(10u16))],
        ] {
            let description = format!("{refused:?}");
            assert!(check_filter(&filter(refused)).is_err(), "{description}");
        }
    }

    /// Everything `bluez::Devices` reads about a device, it finds.
    #[tokio::test]
    async fn serves_what_bluez_devices_reads() {
        let (bus, bluez) = served().await;
        let devices = Devices::new(bus.connect().await.unwrap());
        let path = device_path(PAIR).to_string();

        assert!(devices.connected_airpods().await.unwrap().is_empty());
        bluez.set_connected(PAIR, true).await.unwrap();
        assert_eq!(
            devices.connected_airpods().await.unwrap(),
            std::slice::from_ref(&path)
        );
        assert_eq!(devices.device_address(&path).await.unwrap(), PAIR);
        assert_eq!(devices.device_alias(&path).await, ALIAS);
        assert!(devices.device_is_airpods(&path).await);
        assert!(devices.device_connected(&path).await);

        // An advertiser in range is a device too, but not a paired one.
        Scanner::with_connection(bus.connect().await.unwrap())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        assert_eq!(
            devices.device_aliases().await,
            HashMap::from([(PAIR, ALIAS.to_string())])
        );
        assert!(!devices.device_is_airpods(&device_path(RANDOM)).await);
    }

    /// A connection changing is signalled; setting it as it was is not.
    #[tokio::test]
    async fn connections_reach_the_watch() {
        let (bus, bluez) = served().await;
        let devices = Devices::new(bus.connect().await.unwrap());
        let events = devices.watch_connections().await.unwrap();
        tokio::pin!(events);

        bluez.set_connected(PAIR, true).await.unwrap();
        bluez.set_connected(PAIR, true).await.unwrap();
        bluez.set_connected(PAIR, false).await.unwrap();
        for connected in [true, false] {
            let event = timeout(TIMEOUT, events.next()).await.unwrap().unwrap();
            assert_eq!(event.device_path, device_path(PAIR).as_str());
            assert_eq!(event.connected, Some(connected));
        }
    }

    /// Without the AirPods service a pair is not AirPods to the app, connected
    /// or not; listing it is signalled, and reaches the watch as a change of
    /// services rather than of the connection.
    #[tokio::test]
    async fn services_reach_the_watch() {
        let (bus, bluez) = served().await;
        let devices = Devices::new(bus.connect().await.unwrap());
        let events = devices.watch_connections().await.unwrap();
        tokio::pin!(events);
        let path = device_path(PAIR).to_string();

        bluez.set_airpods_service(PAIR, false).await.unwrap();
        bluez.set_connected(PAIR, true).await.unwrap();
        assert!(!devices.device_is_airpods(&path).await);
        assert!(devices.connected_airpods().await.unwrap().is_empty());

        bluez.set_airpods_service(PAIR, true).await.unwrap();
        assert!(devices.device_is_airpods(&path).await);
        for connected in [Some(true), None] {
            let event = timeout(TIMEOUT, events.next()).await.unwrap().unwrap();
            assert_eq!(event.device_path, path);
            assert_eq!(event.connected, connected);
        }
    }

    /// A new address appears with its data, the next advertisement changes
    /// it, and an address gone quiet goes - and nothing is heard while no one
    /// discovers.
    #[tokio::test]
    async fn advertisements_are_signalled_as_bluetoothd_does() {
        let (bus, bluez) = served().await;
        let client = bus.connect().await.unwrap();
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)
            .unwrap()
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &client, None)
            .await
            .unwrap();

        assert!(
            !bluez.advertise(RANDOM, &[0x01]).await.unwrap(),
            "no discovery"
        );
        Scanner::with_connection(client.clone())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x02, 0x03]).await.unwrap());
        assert!(bluez.advertise(RANDOM, &[0x04, 0x05]).await.unwrap());
        bluez.prune(Instant::now()).await;
        bluez.prune(Instant::now() + TEMPORARY_TIMEOUT).await;

        let path = device_path(RANDOM);
        let mut device_signals = Vec::new();
        while device_signals.len() < 3 {
            let msg = timeout(TIMEOUT, signals.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if msg
                .header()
                .path()
                .is_some_and(|p| p.as_str() != adapter_path())
            {
                device_signals.push(msg);
            }
        }

        let added = &device_signals[0];
        assert_eq!(added.header().member().unwrap().as_str(), "InterfacesAdded");
        let (object, interfaces) = added
            .body()
            .deserialize::<(
                OwnedObjectPath,
                HashMap<String, HashMap<String, OwnedValue>>,
            )>()
            .unwrap();
        assert_eq!(object, path);
        let device = &interfaces["org.bluez.Device1"];
        assert_eq!(apple_data(device), [0x02, 0x03], "the first advertisement");
        assert_eq!(
            String::try_from(device["Address"].clone()).unwrap(),
            RANDOM.to_string()
        );
        assert_eq!(
            String::try_from(device["AddressType"].clone()).unwrap(),
            "random"
        );

        let changed = &device_signals[1];
        assert_eq!(
            changed.header().member().unwrap().as_str(),
            "PropertiesChanged"
        );
        assert_eq!(changed.header().path().unwrap().as_str(), path.as_str());
        let (iface, props, _) = changed
            .body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            .unwrap();
        assert_eq!(iface, "org.bluez.Device1");
        assert_eq!(apple_data(&props), [0x04, 0x05]);

        let removed = &device_signals[2];
        assert_eq!(
            removed.header().member().unwrap().as_str(),
            "InterfacesRemoved"
        );
        let (object, interfaces) = removed
            .body()
            .deserialize::<(OwnedObjectPath, Vec<String>)>()
            .unwrap();
        assert_eq!(object, path);
        assert_eq!(interfaces, ["org.bluez.Device1"]);
    }

    /// The scanner takes what the fake signals: the plumbing the pipeline
    /// tests (#73) build on.
    #[tokio::test]
    async fn the_scanner_reads_its_advertisements() {
        let (bus, bluez) = served().await;
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        scanner.start_discovery().await.unwrap();
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);

        // Twice from the one address: once as the object appears, once as a
        // change, so that this holds whichever of them the scanner reads.
        let pods = Pods::new(PRO_3, PAIR);
        let address = pods.address_at(Duration::ZERO);
        for ms in [0, 1] {
            let ad = pods.advertisement(Duration::from_millis(ms));
            bluez.advertise(address, &ad.data).await.unwrap();
        }
        let advert = timeout(TIMEOUT, adverts.next()).await.unwrap().unwrap();
        assert_eq!(advert.ble_mac, address);
        assert_eq!(advert.data.product_id, PRO_3);
    }

    /// Discovery is per client: each starts and stops its own, and one that
    /// leaves the bus stops its own by leaving.
    #[tokio::test]
    async fn discovery_is_per_client() {
        let (bus, bluez) = served().await;
        let (a, b) = (bus.connect().await.unwrap(), bus.connect().await.unwrap());
        let path = adapter_path();
        let adapter_b = proxy(&b, path, "org.bluez.Adapter1").await;
        {
            let adapter_a = proxy(&a, path, "org.bluez.Adapter1").await;
            adapter_a.call_method("StartDiscovery", &()).await.unwrap();
            assert_eq!(
                error_name(adapter_a.call_method("StartDiscovery", &()).await),
                "org.bluez.Error.InProgress"
            );
        }
        assert_eq!(
            error_name(adapter_b.call_method("StopDiscovery", &()).await),
            "org.bluez.Error.Failed"
        );
        assert!(bluez.is_discovering());
        assert!(adapter_b.get_property::<bool>("Discovering").await.unwrap());

        drop(a);
        eventually(|| !bluez.is_discovering()).await;
        assert!(!adapter_b.get_property::<bool>("Discovering").await.unwrap());
        assert!(!bluez.advertise(RANDOM, &[0x07]).await.unwrap());
    }

    /// The provider's batteries reach `Battery1` on the device, where GNOME
    /// Settings reads them, and follow every change.
    #[tokio::test]
    async fn reads_batteries_from_the_provider() {
        let (bus, bluez) = served().await;
        let mut provider = BatteryProvider::with_connection(bus.connect().await.unwrap())
            .await
            .unwrap();
        let reader = bus.connect().await.unwrap();
        let path = device_path(PAIR).to_string();
        let battery = proxy(&reader, &path, "org.bluez.Battery1").await;

        // No such device: ignored, as bluetoothd ignores it. Batteries are
        // read in order, so it has been dealt with once PAIR's shows.
        let nobody = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x99]);
        let actions = [
            BatteryAction::Add {
                mac: nobody,
                device_path: device_path(nobody).to_string(),
                percentage: 50,
            },
            BatteryAction::Add {
                mac: PAIR,
                device_path: path.clone(),
                percentage: 73,
            },
        ];
        for action in &actions {
            provider.apply(action).await.unwrap();
        }
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 73)])).await;
        assert_eq!(battery.get_property::<u8>("Percentage").await.unwrap(), 73);
        assert_eq!(
            battery.get_property::<String>("Source").await.unwrap(),
            "LinuxPods"
        );

        let update = BatteryAction::Update {
            mac: PAIR,
            percentage: 60,
        };
        provider.apply(&update).await.unwrap();
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 60)])).await;
        assert_eq!(battery.get_property::<u8>("Percentage").await.unwrap(), 60);

        provider
            .apply(&BatteryAction::Remove { mac: PAIR })
            .await
            .unwrap();
        eventually(|| bluez.batteries().is_empty()).await;
        assert!(battery.get_property::<u8>("Percentage").await.is_err());
    }

    /// A provider that leaves the bus takes its batteries along.
    #[tokio::test]
    async fn a_provider_leaving_takes_its_batteries() {
        let (bus, bluez) = served().await;
        let mut provider = BatteryProvider::with_connection(bus.connect().await.unwrap())
            .await
            .unwrap();
        let add = BatteryAction::Add {
            mac: PAIR,
            device_path: device_path(PAIR).to_string(),
            percentage: 73,
        };
        provider.apply(&add).await.unwrap();
        eventually(|| !bluez.batteries().is_empty()).await;

        drop(provider);
        eventually(|| bluez.batteries().is_empty()).await;
    }

    #[tokio::test]
    async fn a_provider_registers_once() {
        let (bus, _bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        let manager = proxy(&conn, adapter_path(), "org.bluez.BatteryProviderManager1").await;
        let root = ObjectPath::try_from("/provider").unwrap();
        let other = ObjectPath::try_from("/other").unwrap();

        manager
            .call_method("RegisterBatteryProvider", &(&root,))
            .await
            .unwrap();
        assert_eq!(
            error_name(
                manager
                    .call_method("RegisterBatteryProvider", &(&root,))
                    .await
            ),
            "org.bluez.Error.AlreadyExists"
        );
        assert_eq!(
            error_name(
                manager
                    .call_method("UnregisterBatteryProvider", &(&other,))
                    .await
            ),
            "org.bluez.Error.DoesNotExist"
        );
        manager
            .call_method("UnregisterBatteryProvider", &(&root,))
            .await
            .unwrap();
    }
}
