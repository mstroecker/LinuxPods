//! A fake BlueZ: `org.bluez` on a private bus, for the app's scanner and
//! battery provider to talk to in place of bluetoothd (#72).
//!
//! It serves what `ble::scanner` and `bluez` call and listen to, and answers
//! there as bluetoothd does, errors included:
//!
//! - the ObjectManager at `/`, which `bluez` lists adapters and devices from
//! - each adapter at `/org/bluez/<name>`, named as [`FakeBluez::add_adapter`]
//!   names it - `hci0`, `hci1` and on: `Adapter1`, whose discovery is per
//!   client and needs the adapter powered, and a `BatteryProviderManager1` of
//!   its own
//! - a `Device1` for each paired pair, under the adapter it is paired with, at
//!   its real address: `Address`, `Adapter`, `Alias`, `Connected`, `Paired`
//!   and `UUIDs`, the AirPods service among them unless
//!   [`FakeBluez::set_airpods_service`] took it out
//! - a `Device1` for each random address an adapter heard an advertisement
//!   from, under that adapter, with its `ManufacturerData`
//! - `Battery1` on a device once a registered provider gives it a level - the
//!   battery GNOME Settings shows
//!
//! Nothing else of bluetoothd is here - no pairing, no `Device1.Connect`, no
//! agent - since the app uses none of it.
//!
//! # Adapters coming and going
//!
//! What bluetoothd does to its clients when an adapter or bluetoothd itself
//! goes, which the app has to recover from (#50):
//!
//! - [`FakeBluez::set_powered`] switches an adapter on or off, signalling
//!   `Powered`. Off, it does what bluetoothd's `adapter_stop` does: every
//!   client's discovery there ends, with `Discovering` signalled; the devices
//!   it only heard advertising go; and its connected devices disconnect. Its
//!   paired devices and the providers registered with it stay, as they do in
//!   bluetoothd. Nothing connects to a device on an adapter that is off.
//! - [`FakeBluez::add_adapter`] and [`FakeBluez::remove_adapter`] plug one in
//!   and out at any time, with `InterfacesAdded` and `InterfacesRemoved`.
//!   Removed, an adapter is first stopped as when it powers off, and takes
//!   its devices, its providers and their batteries along. Its pairings are
//!   kept, as bluetoothd keeps them on disk: plugged in again, it has its
//!   paired devices back, disconnected.
//! - [`FakeBluez::restart`] is bluetoothd restarting without powering its
//!   adapters off, as after a crash: it forgets every client's discovery and
//!   providers, and the devices it only heard advertising, lets go of
//!   `org.bluez`, and takes it again on a connection of its own - a new
//!   unique name, as a new bluetoothd has - so `NameOwnerChanged` says so
//!   twice. The adapters and paired devices are back as they were,
//!   connections and all. [`FakeBluez::stop`] and [`FakeBluez::start`] are
//!   its two halves, for a test to do something in between; on a bus that
//!   activates `org.bluez` ([`super::PrivateBus::activating`]), a call made
//!   in between waits for the new one, as under systemd. A `systemctl
//!   restart` powers the adapters off first, as `set_powered` does.
//!
//!   It is a crash, and a narrow one: only `NameOwnerChanged` tells, and
//!   nothing is announced again. A real bluetoothd also announces every
//!   object again with `InterfacesAdded`. A test passing on a restart proves
//!   the `NameOwnerChanged` path only.
//! - [`FakeBluez::set_discovery_busy`] has an adapter refuse every client's
//!   discovery start as bluetoothd does while the kernel is busy - as it is
//!   for a few seconds after a `kill -9`, the dead bluetoothd's discovery
//!   still running in the controller (#123): `InProgress`, the very error a
//!   client that discovers already gets, and the client not taken on. It
//!   outlasts a restart, as that discovery does, until cleared.
//!
//! # Advertisements
//!
//! Signalled as bluetoothd's source has it, until the captures of #71 confirm
//! or correct that:
//!
//! - An address an adapter hears for the first time becomes a device object
//!   under that adapter, announced by `InterfacesAdded` with its properties,
//!   `ManufacturerData` among them. bluetoothd fills the object in before it
//!   announces it, so no `PropertiesChanged` follows for that first
//!   advertisement.
//! - Each later advertisement from the address is a `PropertiesChanged` of
//!   `ManufacturerData` on `org.bluez.Device1`.
//! - An address not heard for [`TEMPORARY_TIMEOUT`] goes with
//!   `InterfacesRemoved`.
//!
//! An adapter hears advertisements only while some client has discovery on
//! there, as it hears them only while it scans. Every adapter that does hears
//! every advertisement, as two radios in one room both would, and each makes
//! a device of its own of the address. A pair's paired device stays on the
//! adapter it is paired with: its advertisements come from a random address
//! no adapter resolves to it.
//!
//! # Batteries
//!
//! Kept as bluetoothd's `src/battery.c` keeps them. Each adapter's manager has
//! providers of its own: a root registered with one is not registered with
//! another, and the same root registered twice with one is refused, whoever
//! registers it. A provider registers the root of its objects, and the fake
//! reads them from there as bluetoothd does: the ObjectManager's objects at
//! once, then its `InterfacesAdded`, `InterfacesRemoved` and
//! `PropertiesChanged`. A battery takes its device from `Device` when it
//! appears. It is ignored, with a warning, if that is no paired device on the
//! provider's adapter - one on another adapter, an advertiser, or nothing at
//! all - or if another provider got there first. A client that leaves the bus
//! takes its providers' batteries along, and its discovery stops.

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
use crate::bluez::{AIRPODS_UUID, BLUEZ_SERVICE};

/// How long bluetoothd keeps a device it has only heard advertise once it
/// stops hearing it: `TemporaryTimeout` in its main.conf, 30 seconds unless
/// set otherwise.
pub const TEMPORARY_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Where bluetoothd puts the adapter it calls `name`: `/org/bluez/hci0`.
pub fn adapter_path(name: &str) -> String {
    format!("/org/bluez/{name}")
}

/// Where bluetoothd puts a device on the adapter called `adapter`:
/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
pub fn device_path(adapter: &str, address: Address) -> OwnedObjectPath {
    let path = format!(
        "{}/dev_{}",
        adapter_path(adapter),
        address.to_string().replace(':', "_")
    );
    OwnedObjectPath::try_from(path).expect("an adapter's name and a MAC make a valid path")
}

/// The adapter's object, as a device's `Adapter` names it.
fn adapter_object(name: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(adapter_path(name)).expect("an adapter's name makes a valid path")
}

/// The adapter and the address in a device path, if it is one.
fn device_of(path: &str) -> Option<(&str, Address)> {
    let (adapter, device) = path.strip_prefix("/org/bluez/")?.split_once('/')?;
    let address = device
        .strip_prefix("dev_")?
        .replace('_', ":")
        .parse()
        .ok()?;
    Some((adapter, address))
}

/// Whether `name` can name an adapter: one element of an object path, as
/// bluetoothd's `hci0` is.
fn is_adapter_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The address of the adapter added `index`th, from the top of the block RFC
/// 7042 sets aside for documentation - the scenarios' pairs are from its
/// bottom - so it is nobody's: `00:00:5E:00:53:F0` for the first.
fn adapter_address(index: usize) -> Option<Address> {
    let last = u8::try_from(index).ok()?.checked_add(0xF0)?;
    Some(Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, last]))
}

/// bluetoothd's errors, by the names it gives them.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum BluezError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InvalidArguments(String),
    InProgress(String),
    NotReady(String),
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

/// A fake `org.bluez`. Clones share it.
#[derive(Clone)]
pub struct FakeBluez {
    /// The connection it serves on: a new one, with a unique name of its
    /// own, each time it restarts, as a new bluetoothd has.
    conn: Arc<Mutex<Connection>>,
    state: Arc<Mutex<State>>,
    /// Held while a device's `Battery1` is brought in line with its battery,
    /// so two updates cannot interleave.
    exports: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct State {
    /// The adapters' names, in the order they were added.
    adapters: Vec<String>,
    /// The adapters that are powered.
    powered: HashSet<String>,
    /// The address of every adapter ever added, by name: one plugged in
    /// again is the same controller.
    addresses: HashMap<String, Address>,
    /// Paired devices, with the adapter each is paired with. Kept while the
    /// adapter is out, as bluetoothd keeps pairings on disk.
    paired: HashMap<Address, Pairing>,
    /// Random addresses heard advertising, with the adapter that heard each,
    /// and when it last did.
    advertisers: HashMap<(String, Address), Instant>,
    /// The adapters whose BlueZ has no battery API: no
    /// `BatteryProviderManager1` - see [`FakeBluez::drop_battery_api`].
    no_battery_api: HashSet<String>,
    /// The task that sees clients leave the bus, for the connection served
    /// on now.
    watcher: Option<AbortHandle>,
    /// The clients that have discovery on, by adapter.
    discovering: HashMap<String, HashSet<OwnedUniqueName>>,
    /// The adapters whose discovery starts are refused as a busy kernel's
    /// are - see [`FakeBluez::set_discovery_busy`] - with how many each has
    /// refused so far, by adapter.
    busy: HashMap<String, usize>,
    /// Registered providers, with the task reading each.
    providers: HashMap<Provider, AbortHandle>,
    /// The battery each device has from a provider, by device path.
    batteries: HashMap<OwnedObjectPath, Battery>,
    /// Every battery a manager ignored, once, in the order it did: its
    /// adapter, and the `Device` the battery named.
    ignored: Vec<(String, OwnedObjectPath)>,
    /// The device of every battery a provider withdrew, in the order it did.
    withdrawn: Vec<Address>,
}

/// A paired device's pairing: the adapter it is with, and its name.
struct Pairing {
    adapter: String,
    alias: String,
}

/// A battery provider: the adapter whose manager took it, who registered it,
/// and its root path.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Provider {
    adapter: String,
    owner: OwnedUniqueName,
    root: OwnedObjectPath,
}

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
    fn has_adapter(&self, name: &str) -> bool {
        self.adapters.iter().any(|a| a == name)
    }

    fn is_discovering(&self, adapter: &str) -> bool {
        self.discovering
            .get(adapter)
            .is_some_and(|clients| !clients.is_empty())
    }

    /// The path of a paired device whose adapter is here.
    fn paired_path(&self, address: Address) -> Result<OwnedObjectPath> {
        let pairing = self
            .paired
            .get(&address)
            .with_context(|| format!("no paired device {address}"))?;
        anyhow::ensure!(
            self.has_adapter(&pairing.adapter),
            "{address} is paired with {}, which is gone",
            pairing.adapter
        );
        Ok(device_path(&pairing.adapter, address))
    }

    /// The address of the device at `path`, if it is a paired device on
    /// `adapter`.
    fn paired_on(&self, adapter: &str, path: &str) -> Option<Address> {
        let (on, address) = device_of(path)?;
        let pairing = self.paired.get(&address)?;
        (on == adapter && pairing.adapter == adapter).then_some(address)
    }

    /// The devices paired with the adapter called `adapter`.
    fn paired_with(&self, adapter: &str) -> Vec<Address> {
        self.paired
            .iter()
            .filter(|(_, pairing)| pairing.adapter == adapter)
            .map(|(address, _)| *address)
            .collect()
    }

    /// Forgets every client's discovery on the adapters `adapter` picks,
    /// returning the adapters that were discovering.
    fn end_discovery(&mut self, adapter: impl Fn(&str) -> bool) -> Vec<String> {
        let ended: Vec<String> = self
            .discovering
            .iter()
            .filter(|(name, clients)| adapter(name) && !clients.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        self.discovering.retain(|name, _| !adapter(name));
        ended
    }

    /// Forgets the devices the adapters `adapter` picks only heard
    /// advertising, returning their paths.
    fn forget_advertisers(&mut self, adapter: impl Fn(&str) -> bool) -> Vec<OwnedObjectPath> {
        let heard: Vec<OwnedObjectPath> = self
            .advertisers
            .keys()
            .filter(|(heard_by, _)| adapter(heard_by))
            .map(|(heard_by, address)| device_path(heard_by, *address))
            .collect();
        self.advertisers
            .retain(|(heard_by, _), _| !adapter(heard_by));
        heard
    }

    /// Forgets the providers `provider` picks, and their batteries, returning
    /// the batteries' device paths.
    fn forget_providers(&mut self, provider: impl Fn(&Provider) -> bool) -> Vec<OwnedObjectPath> {
        self.providers.retain(|p, task| {
            let gone = provider(p);
            if gone {
                task.abort();
            }
            !gone
        });
        self.take_batteries(provider)
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
    /// Serves the ObjectManager on `conn`, with no adapter yet - see
    /// [`FakeBluez::add_adapter`]. The name comes later, from
    /// [`FakeBluez::publish`], so that adapters and devices can be put in
    /// place before the app can find any of it.
    pub async fn serve(conn: Connection) -> Result<Self> {
        let bluez = Self {
            conn: Arc::new(Mutex::new(conn.clone())),
            state: Arc::default(),
            exports: Arc::default(),
        };
        bluez.serve_on(&conn).await?;
        Ok(bluez)
    }

    /// What every connection it serves on has: the ObjectManager at `/`,
    /// and a watch on clients leaving the bus.
    async fn serve_on(&self, conn: &Connection) -> Result<()> {
        conn.object_server()
            .at("/", zbus::fdo::ObjectManager)
            .await?;

        // bluetoothd forgets a client's discovery and providers when it
        // leaves the bus.
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let mut owners = dbus.receive_name_owner_changed().await?;
        let watcher = self.clone();
        let task = tokio::spawn(async move {
            while let Some(signal) = owners.next().await {
                let Ok(args) = signal.args() else { continue };
                if let BusName::Unique(name) = args.name()
                    && args.new_owner().is_none()
                {
                    watcher.client_gone(name.to_owned().into()).await;
                }
            }
        });
        if let Some(old) = self.state().watcher.replace(task.abort_handle()) {
            old.abort();
        }
        Ok(())
    }

    /// Serves an adapter's objects on `conn`: its manager, unless its BlueZ
    /// has no battery API, then `Adapter1`.
    async fn export_adapter(&self, conn: &Connection, name: &str, address: Address) -> Result<()> {
        let server = conn.object_server();
        let path = adapter_object(name);
        // The manager ahead of the adapter, which is what a client waits for:
        // one that registers a provider as soon as the adapter is announced
        // finds the manager there. bluetoothd announces both at once.
        if !self.state().no_battery_api.contains(name) {
            let manager = BatteryProviderManager1 {
                bluez: self.clone(),
                adapter: name.to_string(),
            };
            server.at(&path, manager).await?;
        }
        let adapter = Adapter1 {
            bluez: self.clone(),
            name: name.to_string(),
            address,
        };
        server.at(&path, adapter).await?;
        Ok(())
    }

    /// Takes the battery API away from the adapter called `name`, as a BlueZ
    /// built or run without it has none: no `BatteryProviderManager1`, so
    /// registering a provider fails as the method is unknown. Before the
    /// app starts, as such a BlueZ never had one.
    pub async fn drop_battery_api(&self, name: &str) -> Result<()> {
        anyhow::ensure!(self.state().has_adapter(name), "there is no adapter {name}");
        self.state().no_battery_api.insert(name.to_string());
        self.conn()
            .object_server()
            .remove::<BatteryProviderManager1, _>(adapter_object(name))
            .await?;
        Ok(())
    }

    /// Adds an adapter at `/org/bluez/<name>`, as bluetoothd adds one for each
    /// controller it finds: `Adapter1`, powered or not, and a
    /// `BatteryProviderManager1` of its own. bluetoothd names them `hci0`,
    /// `hci1` and on; any name that makes one path element will do.
    ///
    /// Signalled with `InterfacesAdded` once the name is taken, as a USB
    /// dongle plugged in is. One that was here before is the same controller,
    /// and has the devices paired with it back, disconnected.
    pub async fn add_adapter(&self, name: &str, powered: bool) -> Result<()> {
        anyhow::ensure!(is_adapter_name(name), "{name:?} cannot name an adapter");
        let (address, paired) = {
            let mut state = self.state();
            anyhow::ensure!(
                !state.has_adapter(name),
                "there is an adapter {name} already"
            );
            let address = match state.addresses.get(name) {
                Some(&address) => address,
                None => {
                    let address =
                        adapter_address(state.addresses.len()).context("too many adapters")?;
                    state.addresses.insert(name.to_string(), address);
                    address
                }
            };
            state.adapters.push(name.to_string());
            if powered {
                state.powered.insert(name.to_string());
            }
            let paired: Vec<(Address, String)> = state
                .paired
                .iter()
                .filter(|(_, pairing)| pairing.adapter == name)
                .map(|(address, pairing)| (*address, pairing.alias.clone()))
                .collect();
            (address, paired)
        };
        self.export_adapter(&self.conn(), name, address).await?;
        // Loaded from its storage once it is there, as bluetoothd does.
        for (address, alias) in paired {
            self.export_paired(name, address, alias).await?;
        }
        Ok(())
    }

    /// Unplugs the adapter called `name`: it stops as when it powers off, and
    /// its devices, its providers and their batteries go with it, then the
    /// adapter itself, each with `InterfacesRemoved`. Its pairings are kept
    /// for when it is plugged in again.
    pub async fn remove_adapter(&self, name: &str) -> Result<()> {
        anyhow::ensure!(self.state().has_adapter(name), "there is no adapter {name}");
        self.stop_adapter(name).await;
        let (batteries, paired) = {
            let mut state = self.state();
            state.adapters.retain(|a| a != name);
            state.powered.remove(name);
            let batteries = state.forget_providers(|p| p.adapter == name);
            (batteries, state.paired_with(name))
        };
        for device in batteries {
            self.sync_battery(&device).await;
        }
        let conn = self.conn();
        let server = conn.object_server();
        for address in paired {
            server
                .remove::<PairedDevice, _>(device_path(name, address))
                .await?;
        }
        let path = adapter_object(name);
        server.remove::<Adapter1, _>(&path).await?;
        // Not there when its BlueZ has no battery API.
        let _ = server.remove::<BatteryProviderManager1, _>(&path).await;
        Ok(())
    }

    /// Powers the adapter called `name` on or off, signalling `Powered`;
    /// setting what is already set signals nothing. Off, it stops as
    /// bluetoothd's `adapter_stop` does - see the module docs.
    pub async fn set_powered(&self, name: &str, powered: bool) -> Result<()> {
        let changed = {
            let mut state = self.state();
            anyhow::ensure!(state.has_adapter(name), "there is no adapter {name}");
            if powered {
                state.powered.insert(name.to_string())
            } else {
                state.powered.remove(name)
            }
        };
        if !changed {
            return Ok(());
        }
        let iface = self
            .conn()
            .object_server()
            .interface::<_, Adapter1>(adapter_path(name))
            .await?;
        iface
            .get()
            .await
            .powered_changed(iface.signal_emitter())
            .await?;
        // After `Powered`, as bluetoothd signals it before it stops.
        if !powered {
            self.stop_adapter(name).await;
        }
        Ok(())
    }

    /// What bluetoothd's `adapter_stop` does to an adapter's clients: their
    /// discovery there ends, the devices it only heard advertising go, and its
    /// connected devices disconnect, each signalled.
    async fn stop_adapter(&self, name: &str) {
        let (ended, heard, paired) = {
            let mut state = self.state();
            let ended = state.end_discovery(|adapter| adapter == name);
            let heard = state.forget_advertisers(|adapter| adapter == name);
            (ended, heard, state.paired_with(name))
        };
        let conn = self.conn();
        let server = conn.object_server();
        for path in heard {
            let _ = server.remove::<Advertiser, _>(&path).await;
        }
        for address in paired {
            if let Err(e) = self.signal_connected(name, address, false).await {
                tracing::warn!("fake BlueZ: failed to disconnect {address}: {e:#}");
            }
        }
        for adapter in ended {
            let _ = self.signal_discovering(&adapter).await;
        }
    }

    /// bluetoothd restarting without powering its adapters off, as after a
    /// crash - see the module docs: [`FakeBluez::stop`], then
    /// [`FakeBluez::start`] on `conn`, a connection of its own. Back from
    /// here, `org.bluez` is taken again, under a new unique name.
    pub async fn restart(&self, conn: Connection) -> Result<()> {
        self.stop().await?;
        self.start(conn).await
    }

    /// bluetoothd going, as it does when it crashes: every client's
    /// discovery and providers, and the devices only heard advertising, are
    /// forgotten, and `org.bluez` let go of. Until [`FakeBluez::start`], a
    /// call to `org.bluez` finds no one - or, on a bus that activates it,
    /// waits for the new one.
    pub async fn stop(&self) -> Result<()> {
        let (heard, batteries) = {
            let mut state = self.state();
            state.end_discovery(|_| true);
            let heard = state.forget_advertisers(|_| true);
            (heard, state.forget_providers(|_| true))
        };
        // The objects it made for its clients go with it, before the name.
        for device in batteries {
            self.sync_battery(&device).await;
        }
        let conn = self.conn();
        let server = conn.object_server();
        for path in heard {
            let _ = server.remove::<Advertiser, _>(&path).await;
        }
        conn.release_name(BLUEZ_SERVICE)
            .await
            .context("failed to let go of org.bluez")?;
        Ok(())
    }

    /// The new bluetoothd, on `conn`, which has a unique name of its own:
    /// the adapters and paired devices as the old one left them, connections
    /// and all, none discovering. All of it is in place before the name is
    /// taken, so nothing is announced to whoever listens to `org.bluez`.
    pub async fn start(&self, conn: Connection) -> Result<()> {
        let old = self.conn();
        let (adapters, paired) = {
            let state = self.state();
            let adapters: Vec<(String, Address)> = state
                .adapters
                .iter()
                .filter_map(|name| Some((name.clone(), *state.addresses.get(name)?)))
                .collect();
            let paired: Vec<(String, Address)> = state
                .paired
                .iter()
                .filter(|(_, pairing)| state.has_adapter(&pairing.adapter))
                .map(|(address, pairing)| (pairing.adapter.clone(), *address))
                .collect();
            (adapters, paired)
        };
        // As the old one left them.
        let mut devices = Vec::new();
        for (adapter, address) in paired {
            let path = device_path(&adapter, address);
            let iface = old
                .object_server()
                .interface::<_, PairedDevice>(&path)
                .await?;
            let device = iface.get().await;
            devices.push((
                path,
                PairedDevice {
                    address,
                    adapter: device.adapter.clone(),
                    alias: device.alias.clone(),
                    connected: device.connected,
                    airpods_service: device.airpods_service,
                },
            ));
        }
        self.serve_on(&conn).await?;
        for (name, address) in adapters {
            self.export_adapter(&conn, &name, address).await?;
        }
        for (path, device) in devices {
            conn.object_server().at(path, device).await?;
        }
        *self.conn.lock().unwrap_or_else(PoisonError::into_inner) = conn;
        self.publish().await
    }

    /// Takes the name `org.bluez`, under which the app looks for BlueZ.
    pub async fn publish(&self) -> Result<()> {
        self.conn()
            .request_name(BLUEZ_SERVICE)
            .await
            .context("failed to take org.bluez")
    }

    /// The connection it serves on now: a new one after each restart.
    pub fn connection(&self) -> Connection {
        self.conn()
    }

    fn conn(&self) -> Connection {
        self.conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The adapters' names, in the order they were added.
    pub fn adapters(&self) -> Vec<String> {
        self.state().adapters.clone()
    }

    /// Whether the adapter called `adapter` is here and powered.
    pub fn is_powered(&self, adapter: &str) -> bool {
        self.state().powered.contains(adapter)
    }

    /// The devices paired with the adapter called `adapter`, here or not.
    pub fn paired_with(&self, adapter: &str) -> Vec<Address> {
        self.state().paired_with(adapter)
    }

    /// Whether a paired device can connect: its adapter is here and powered.
    pub fn can_connect(&self, address: Address) -> bool {
        let state = self.state();
        state
            .paired
            .get(&address)
            .is_some_and(|pairing| state.powered.contains(&pairing.adapter))
    }

    /// Adds a device paired with the adapter called `adapter`, disconnected.
    pub async fn add_device(&self, adapter: &str, address: Address, alias: &str) -> Result<()> {
        {
            let mut state = self.state();
            anyhow::ensure!(state.has_adapter(adapter), "there is no adapter {adapter}");
            anyhow::ensure!(
                !state.paired.contains_key(&address),
                "{address} is paired already"
            );
            let pairing = Pairing {
                adapter: adapter.to_string(),
                alias: alias.to_string(),
            };
            state.paired.insert(address, pairing);
        }
        self.export_paired(adapter, address, alias.to_string())
            .await
    }

    /// Puts a paired device's object under its adapter, disconnected.
    async fn export_paired(&self, adapter: &str, address: Address, alias: String) -> Result<()> {
        let device = PairedDevice {
            address,
            adapter: adapter_object(adapter),
            alias,
            connected: false,
            airpods_service: true,
        };
        self.conn()
            .object_server()
            .at(device_path(adapter, address), device)
            .await?;
        Ok(())
    }

    /// Connects or disconnects a paired device. A change is signalled, as
    /// bluetoothd signals it; setting what is already set signals nothing.
    /// Nothing connects on an adapter that is off.
    pub async fn set_connected(&self, address: Address, connected: bool) -> Result<()> {
        let path = {
            let state = self.state();
            let path = state.paired_path(address)?;
            anyhow::ensure!(
                !connected
                    || state
                        .paired
                        .get(&address)
                        .is_some_and(|pairing| state.powered.contains(&pairing.adapter)),
                "{address} cannot connect: its adapter is off"
            );
            path
        };
        let (adapter, _) = device_of(path.as_str()).context("a paired device's path")?;
        self.signal_connected(adapter, address, connected).await
    }

    /// Sets a paired device's `Connected` on the adapter called `adapter`,
    /// signalling a change.
    async fn signal_connected(
        &self,
        adapter: &str,
        address: Address,
        connected: bool,
    ) -> Result<()> {
        let iface = self
            .conn()
            .object_server()
            .interface::<_, PairedDevice>(device_path(adapter, address))
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
        let path = self.state().paired_path(address)?;
        let iface = self
            .conn()
            .object_server()
            .interface::<_, PairedDevice>(path)
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

    /// Whether any client has discovery on at the adapter called `adapter`.
    pub fn is_discovering(&self, adapter: &str) -> bool {
        self.state().is_discovering(adapter)
    }

    /// Has the adapter called `adapter` refuse discovery starts, or take them
    /// again, as bluetoothd does while the kernel refuses its own start with
    /// `Busy` (#123). bluetoothd answers the client with
    /// `org.bluez.Error.InProgress` - what `discovery_complete` sends for a
    /// failed start, as `start_discovery` does for a client that has discovery
    /// already - and drops it as a discovery client: so here every
    /// `StartDiscovery` there is refused that way, and the caller is not
    /// taken on. Clients discovering already keep their discovery. Kept
    /// through [`FakeBluez::restart`], as the dead bluetoothd's discovery
    /// outlives it in the controller.
    pub fn set_discovery_busy(&self, adapter: &str, busy: bool) {
        let mut state = self.state();
        if busy {
            state.busy.entry(adapter.to_string()).or_default();
        } else {
            state.busy.remove(adapter);
        }
    }

    /// How many discovery starts the adapter called `adapter` has refused
    /// since [`FakeBluez::set_discovery_busy`] made it busy; 0 while it is
    /// not.
    pub fn busy_refusals(&self, adapter: &str) -> usize {
        self.state().busy.get(adapter).copied().unwrap_or_default()
    }

    /// An advertisement from `address`, `apple_data` the value of its Apple
    /// entry in `ManufacturerData`, heard by every adapter that discovers.
    /// Signalled as the module docs describe: returns whether any adapter
    /// heard it.
    pub async fn advertise(&self, address: Address, apple_data: &[u8]) -> Result<bool> {
        let hearing: Vec<(String, bool)> = {
            let mut state = self.state();
            let discovering: Vec<String> = state
                .adapters
                .iter()
                .filter(|adapter| state.is_discovering(adapter))
                .cloned()
                .collect();
            if discovering.is_empty() {
                return Ok(false);
            }
            anyhow::ensure!(
                !state.paired.contains_key(&address),
                "{address} is a paired device's address"
            );
            let now = Instant::now();
            discovering
                .into_iter()
                .map(|adapter| {
                    let heard_before = state
                        .advertisers
                        .insert((adapter.clone(), address), now)
                        .is_some();
                    (adapter, heard_before)
                })
                .collect()
        };
        for (adapter, heard_before) in hearing {
            self.heard(&adapter, address, apple_data, heard_before)
                .await?;
        }
        Ok(true)
    }

    /// Signals what the adapter called `adapter` heard from `address`: the
    /// device it makes of a new address, or the change to one it heard
    /// before.
    async fn heard(
        &self,
        adapter: &str,
        address: Address,
        apple_data: &[u8],
        heard_before: bool,
    ) -> Result<()> {
        let path = device_path(adapter, address);
        let conn = self.conn();
        let server = conn.object_server();
        let advertiser = || Advertiser {
            address,
            adapter: adapter_object(adapter),
            apple_data: apple_data.to_vec(),
        };
        // `at` announces the object with its properties.
        if !heard_before && server.at(&path, advertiser()).await? {
            return Ok(());
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
        Ok(())
    }

    /// The random addresses the adapter called `adapter` has a device for,
    /// from hearing them advertise.
    pub fn advertisers_on(&self, adapter: &str) -> HashSet<Address> {
        self.state()
            .advertisers
            .keys()
            .filter(|(heard_by, _)| heard_by == adapter)
            .map(|(_, address)| *address)
            .collect()
    }

    /// Removes the devices only heard advertising that have not been heard for
    /// [`TEMPORARY_TIMEOUT`] by `now`.
    pub async fn prune(&self, now: Instant) {
        let stale: Vec<(String, Address)> = {
            let mut state = self.state();
            let stale: Vec<(String, Address)> = state
                .advertisers
                .iter()
                .filter(|(_, heard)| now.saturating_duration_since(**heard) >= TEMPORARY_TIMEOUT)
                .map(|(heard_by, _)| heard_by.clone())
                .collect();
            for heard_by in &stale {
                state.advertisers.remove(heard_by);
            }
            stale
        };
        let conn = self.conn();
        let server = conn.object_server();
        for (adapter, address) in stale {
            let _ = server
                .remove::<Advertiser, _>(device_path(&adapter, address))
                .await;
        }
    }

    /// Every device's battery level, as `Battery1` shows it and GNOME Settings
    /// with it.
    pub fn batteries(&self) -> HashMap<Address, u8> {
        self.batteries_from(|_| true)
    }

    /// The battery levels the manager of the adapter called `adapter` took
    /// from its providers, by device.
    pub fn batteries_on(&self, adapter: &str) -> HashMap<Address, u8> {
        self.batteries_from(|provider| provider.adapter == adapter)
    }

    fn batteries_from(&self, providers: impl Fn(&Provider) -> bool) -> HashMap<Address, u8> {
        self.state()
            .batteries
            .values()
            .filter(|b| providers(&b.provider))
            .filter_map(|b| Some((b.address, b.percentage?)))
            .collect()
    }

    /// Every battery a manager ignored, as bluetoothd warns of each, once: the
    /// manager's adapter, and the `Device` the battery named.
    pub fn ignored_batteries(&self) -> Vec<(String, OwnedObjectPath)> {
        self.state().ignored.clone()
    }

    /// The device of every battery a provider withdrew from its root, by
    /// address, in the order it did - not those that went with their
    /// provider, unregistered or gone.
    pub fn withdrawn_batteries(&self) -> Vec<Address> {
        self.state().withdrawn.clone()
    }

    /// How many providers are registered with the manager of the adapter
    /// called `adapter`.
    pub fn providers_on(&self, adapter: &str) -> usize {
        self.state()
            .providers
            .keys()
            .filter(|provider| provider.adapter == adapter)
            .count()
    }

    fn register_provider(&self, provider: Provider) -> Result<(), BluezError> {
        let mut state = self.state();
        // By the root alone, as bluetoothd has it: whoever registered it.
        if state
            .providers
            .keys()
            .any(|p| p.adapter == provider.adapter && p.root == provider.root)
        {
            return Err(BluezError::AlreadyExists("Provider already exists".into()));
        }
        let task = tokio::spawn(self.clone().read_provider(provider.clone()));
        state.providers.insert(provider, task.abort_handle());
        Ok(())
    }

    async fn unregister_provider(&self, provider: Provider) -> Result<(), BluezError> {
        let devices = {
            let mut state = self.state();
            let Some(task) = state.providers.remove(&provider) else {
                return Err(BluezError::DoesNotExist("Provider does not exist".into()));
            };
            task.abort();
            state.take_batteries(|p| *p == provider)
        };
        for device in devices {
            self.sync_battery(&device).await;
        }
        Ok(())
    }

    /// A connection left the bus: its discovery stops on every adapter, and
    /// its providers go.
    async fn client_gone(&self, name: OwnedUniqueName) {
        let (stopped, devices) = {
            let mut state = self.state();
            let mut stopped = Vec::new();
            for (adapter, clients) in &mut state.discovering {
                if clients.remove(&name) && clients.is_empty() {
                    stopped.push(adapter.clone());
                }
            }
            (stopped, state.forget_providers(|p| p.owner == name))
        };
        for adapter in stopped {
            let _ = self.signal_discovering(&adapter).await;
        }
        for device in devices {
            self.sync_battery(&device).await;
        }
    }

    /// Signals an adapter's `Discovering` from outside an `Adapter1` method.
    async fn signal_discovering(&self, adapter: &str) -> zbus::Result<()> {
        let iface = self
            .conn()
            .object_server()
            .interface::<_, Adapter1>(adapter_path(adapter))
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
                "fake BlueZ: stopped reading battery provider {} at {} on {}: {e}",
                provider.owner,
                provider.root,
                provider.adapter
            );
        }
    }

    async fn try_read_provider(&self, provider: &Provider) -> zbus::Result<()> {
        let Provider { owner, root, .. } = provider;
        let manager = zbus::fdo::ObjectManagerProxy::builder(&self.conn())
            .destination(owner.as_ref())?
            .path(root.as_ref())?
            .build()
            .await?;
        // Subscribed before the objects are read, so nothing falls in between.
        // One stream for every signal, so that they are taken in the order the
        // provider sent them, as bluetoothd takes them. From a stream each, a
        // battery withdrawn and offered again within a moment could be taken
        // the other way round - and the offer refused as provided already.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(owner.as_ref())?
            .path_namespace(root.as_ref())?
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &self.conn(), None).await?;

        for (object, interfaces) in manager.get_managed_objects().await? {
            let props = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == PROVIDER_INTERFACE);
            if let Some((_, props)) = props {
                let provided = Provided::read(props.iter().map(|(k, v)| (k.as_str(), &**v)));
                self.battery_added(provider, object, provided).await;
            }
        }

        while let Some(msg) = signals.next().await {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            let interface = header.interface().map(|i| i.as_str());
            let member = header.member().map(|m| m.as_str());
            let body = msg.body();
            match (interface, member) {
                (Some("org.freedesktop.DBus.ObjectManager"), Some("InterfacesAdded")) => {
                    type Added = (
                        OwnedObjectPath,
                        HashMap<String, HashMap<String, OwnedValue>>,
                    );
                    let Ok((object, interfaces)) = body.deserialize::<Added>() else {
                        continue;
                    };
                    if let Some(props) = interfaces.get(PROVIDER_INTERFACE) {
                        let provided =
                            Provided::read(props.iter().map(|(k, v)| (k.as_str(), &**v)));
                        self.battery_added(provider, object, provided).await;
                    }
                }
                (Some("org.freedesktop.DBus.ObjectManager"), Some("InterfacesRemoved")) => {
                    let Ok((object, interfaces)) =
                        body.deserialize::<(OwnedObjectPath, Vec<String>)>()
                    else {
                        continue;
                    };
                    if interfaces.iter().any(|name| name == PROVIDER_INTERFACE) {
                        self.battery_removed(provider, &object).await;
                    }
                }
                (Some("org.freedesktop.DBus.Properties"), Some("PropertiesChanged")) => {
                    let Some(object) = header.path().map(|p| p.to_owned()) else {
                        continue;
                    };
                    type Changed = (String, HashMap<String, OwnedValue>, Vec<String>);
                    let Ok((iface, props, _)) = body.deserialize::<Changed>() else {
                        continue;
                    };
                    if iface == PROVIDER_INTERFACE {
                        let provided =
                            Provided::read(props.iter().map(|(k, v)| (k.as_str(), &**v)));
                        self.battery_changed(provider, &object, provided.percentage)
                            .await;
                    }
                }
                _ => {}
            }
        }
        Ok(())
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
            // bluetoothd looks the device up among the manager's adapter's
            // own, and leaves out those only heard advertising.
            let Some(address) = state.paired_on(&provider.adapter, &device) else {
                tracing::warn!(
                    "fake BlueZ: {} ignoring battery for non-existent device path {device}",
                    provider.adapter
                );
                // An object exported while the provider's objects were first
                // read is read twice: in the list, and as it is added.
                let ignored = (provider.adapter.clone(), device);
                if !state.ignored.contains(&ignored) {
                    state.ignored.push(ignored);
                }
                return;
            };
            if state.batteries.contains_key(&device) {
                tracing::warn!("fake BlueZ: battery {device} is already provided");
                return;
            }
            let battery = Battery {
                address,
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
            if let Some(device) = &device
                && let Some(battery) = state.batteries.remove(device)
            {
                state.withdrawn.push(battery.address);
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
        let conn = self.conn();
        let server = conn.object_server();
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
    name: String,
    address: Address,
}

impl Adapter1 {
    /// bluetoothd refuses discovery calls on an adapter that is off.
    fn ready(&self) -> Result<(), BluezError> {
        if self.powered() {
            Ok(())
        } else {
            Err(BluezError::NotReady("Resource Not Ready".into()))
        }
    }
}

#[interface(name = "org.bluez.Adapter1")]
impl Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<String, OwnedValue>) -> Result<(), BluezError> {
        self.ready()?;
        check_filter(&filter).map_err(BluezError::InvalidArguments)
    }

    async fn start_discovery(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), BluezError> {
        self.ready()?;
        let client = sender(&header)?;
        let in_progress = || BluezError::InProgress("Operation already in progress".into());
        let started = {
            let mut state = self.bluez.state();
            if let Some(refused) = state.busy.get_mut(&self.name) {
                *refused += 1;
                return Err(in_progress());
            }
            let clients = state.discovering.entry(self.name.clone()).or_default();
            let idle = clients.is_empty();
            if !clients.insert(client) {
                return Err(in_progress());
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
        self.ready()?;
        let client = sender(&header)?;
        let stopped = {
            let mut state = self.bluez.state();
            let clients = state.discovering.entry(self.name.clone()).or_default();
            if !clients.remove(&client) {
                return Err(BluezError::Failed("No discovery started".into()));
            }
            clients.is_empty()
        };
        if stopped {
            self.discovering_changed(&emitter).await?;
        }
        Ok(())
    }

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
        "fakepods".to_string()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        self.name()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        self.bluez.is_powered(&self.name)
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.bluez.is_discovering(&self.name)
    }
}

/// One adapter's manager, keeping the providers registered with it.
struct BatteryProviderManager1 {
    bluez: FakeBluez,
    adapter: String,
}

impl BatteryProviderManager1 {
    fn provider(&self, root: OwnedObjectPath, header: &Header<'_>) -> Result<Provider, BluezError> {
        Ok(Provider {
            adapter: self.adapter.clone(),
            owner: sender(header)?,
            root,
        })
    }
}

#[interface(name = "org.bluez.BatteryProviderManager1")]
impl BatteryProviderManager1 {
    fn register_battery_provider(
        &self,
        provider: OwnedObjectPath,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BluezError> {
        self.bluez
            .register_provider(self.provider(provider, &header)?)
    }

    async fn unregister_battery_provider(
        &self,
        provider: OwnedObjectPath,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BluezError> {
        let provider = self.provider(provider, &header)?;
        self.bluez.unregister_provider(provider).await
    }
}

/// A pair paired with an adapter, at its real address.
struct PairedDevice {
    address: Address,
    adapter: OwnedObjectPath,
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
        self.adapter.clone()
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

/// A random address an adapter heard advertising: what bluetoothd makes of a
/// pair it cannot resolve, or of anything else in range.
struct Advertiser {
    address: Address,
    /// The adapter that heard it.
    adapter: OwnedObjectPath,
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
        self.adapter.clone()
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
    use crate::bluez::{self, BatteryAction, BatteryProvider, Devices};
    use crate::harness::PrivateBus;
    use crate::harness::pods::{PRO_3, Pods};

    /// Plenty for any of these, and a hang fails the test instead of CI.
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// The adapter of most tests, and the one a second adapter is added as.
    const ADAPTER: &str = "hci0";
    const SECOND: &str = "hci1";

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);
    const ALIAS: &str = "Fake AirPods";
    /// A random address, as an advertisement comes from.
    const RANDOM: Address = Address::new([0x4A, 0x11, 0x22, 0x33, 0x44, 0x55]);

    /// A fake BlueZ on a bus of its own with one powered adapter and one pair
    /// paired with it, under its name.
    async fn served() -> (PrivateBus, FakeBluez) {
        let bus = PrivateBus::start().unwrap();
        let bluez = FakeBluez::serve(bus.connect().await.unwrap())
            .await
            .unwrap();
        bluez.add_adapter(ADAPTER, true).await.unwrap();
        bluez.add_device(ADAPTER, PAIR, ALIAS).await.unwrap();
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
    fn device_paths_name_the_adapter_and_the_address() {
        let path = device_path(ADAPTER, PAIR);
        assert_eq!(path.as_str(), "/org/bluez/hci0/dev_00_00_5E_00_53_01");
        assert_eq!(device_of(&path), Some((ADAPTER, PAIR)));
        assert_eq!(
            device_of("/org/bluez/hci1/dev_00_00_5E_00_53_01"),
            Some((SECOND, PAIR))
        );
        assert_eq!(device_of("/org/bluez/hci0"), None);
        assert_eq!(
            device_of("/org/bluez/hci0/dev_00_00_5E_00_53_01/service0001"),
            None
        );
    }

    #[test]
    fn adapters_are_named_as_one_path_element() {
        for name in ["hci0", "hci12", "usb_dongle"] {
            assert!(is_adapter_name(name), "{name}");
        }
        for name in ["", "hci-1", "hci0/dev", "/hci0"] {
            assert!(!is_adapter_name(name), "{name:?}");
        }
        assert_ne!(adapter_address(0), adapter_address(1));
        assert_eq!(adapter_address(16), None);
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
        let path = device_path(ADAPTER, PAIR).to_string();

        assert!(devices.connected_airpods().await.unwrap().is_empty());
        bluez.set_connected(PAIR, true).await.unwrap();
        assert_eq!(
            devices.connected_airpods().await.unwrap(),
            std::slice::from_ref(&path)
        );
        let (address, adapter) = devices.device_address_and_adapter(&path).await.unwrap();
        assert_eq!(address, PAIR);
        assert_eq!(adapter.unwrap(), adapter_path(ADAPTER));
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
        assert!(
            !devices
                .device_is_airpods(device_path(ADAPTER, RANDOM).as_str())
                .await
        );
    }

    /// `bluez::adapters` finds every adapter, whatever it is called, and
    /// whether it is powered.
    #[tokio::test]
    async fn adapters_are_found_powered_or_not() {
        let bus = PrivateBus::start().unwrap();
        let bluez = FakeBluez::serve(bus.connect().await.unwrap())
            .await
            .unwrap();
        bluez.add_adapter(SECOND, true).await.unwrap();
        bluez.add_adapter(ADAPTER, false).await.unwrap();
        bluez.add_device(SECOND, PAIR, ALIAS).await.unwrap();
        bluez.publish().await.unwrap();

        let adapters = bluez::adapters(&bus.connect().await.unwrap())
            .await
            .unwrap();
        let found: Vec<_> = adapters
            .iter()
            .map(|a| (a.path.as_str(), a.powered))
            .collect();
        assert_eq!(
            found,
            [("/org/bluez/hci0", false), ("/org/bluez/hci1", true)]
        );
        assert!(
            bluez.add_adapter(SECOND, true).await.is_err(),
            "there already"
        );
        assert!(bluez.add_adapter("hci-2", true).await.is_err(), "no name");
        assert!(
            bluez.add_device("hci2", OTHER, ALIAS).await.is_err(),
            "no adapter"
        );
    }

    /// Discovery is refused on an adapter that is off, as bluetoothd refuses
    /// it.
    #[tokio::test]
    async fn discovery_needs_the_adapter_powered() {
        let (bus, bluez) = served().await;
        bluez.add_adapter(SECOND, false).await.unwrap();
        let conn = bus.connect().await.unwrap();
        let path = adapter_path(SECOND);
        let adapter = proxy(&conn, &path, "org.bluez.Adapter1").await;

        assert!(!adapter.get_property::<bool>("Powered").await.unwrap());
        let filter = HashMap::from([("Transport", Value::from("le"))]);
        for result in [
            adapter.call_method("SetDiscoveryFilter", &(filter,)).await,
            adapter.call_method("StartDiscovery", &()).await,
            adapter.call_method("StopDiscovery", &()).await,
        ] {
            assert_eq!(error_name(result), "org.bluez.Error.NotReady");
        }
        assert!(!bluez.is_discovering(SECOND));
    }

    /// The scanner starts discovery on every powered adapter, and one that
    /// refuses leaves the others scanning. One that is off it leaves alone.
    #[tokio::test]
    async fn the_scanner_scans_on_every_powered_adapter() {
        let (bus, bluez) = served().await;
        bluez.add_adapter(SECOND, true).await.unwrap();
        bluez.add_adapter("hci2", false).await.unwrap();
        // Refused on the first adapter, as this client discovers there already.
        let conn = bus.connect().await.unwrap();
        let first = adapter_path(ADAPTER);
        proxy(&conn, &first, "org.bluez.Adapter1")
            .await
            .call_method("StartDiscovery", &())
            .await
            .unwrap();

        let scanner = Scanner::with_connection(conn);
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        assert_eq!(
            scanner.start_discovery().await.unwrap(),
            [adapter_path(SECOND)]
        );
        assert!(bluez.is_discovering(SECOND));
        assert!(!bluez.is_discovering("hci2"));

        let pods = Pods::new(PRO_3, PAIR);
        let ad = pods.advertisement(Duration::ZERO);
        assert!(bluez.advertise(ad.address, &ad.data).await.unwrap());
        let advert = timeout(TIMEOUT, adverts.next()).await.unwrap().unwrap();
        assert_eq!(advert.ble_mac, ad.address);
        assert!(bluez.advertisers_on(SECOND).contains(&ad.address));
        assert!(bluez.advertisers_on("hci2").is_empty(), "heard while off");
    }

    /// A busy adapter refuses a discovery start with `InProgress`, as
    /// bluetoothd does when the kernel refuses its own (#123), and does not
    /// take the caller on: a stop is refused as there is nothing to stop, and
    /// nothing is heard. Through a restart and until cleared; then a start is
    /// taken again.
    #[tokio::test]
    async fn a_busy_adapter_refuses_discovery_without_taking_the_client() {
        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        let path = adapter_path(ADAPTER);
        let adapter = proxy(&conn, &path, "org.bluez.Adapter1").await;

        bluez.set_discovery_busy(ADAPTER, true);
        bluez.restart(bus.connect().await.unwrap()).await.unwrap();
        assert_eq!(
            error_name(adapter.call_method("StartDiscovery", &()).await),
            "org.bluez.Error.InProgress"
        );
        assert_eq!(bluez.busy_refusals(ADAPTER), 1);
        assert!(!bluez.is_discovering(ADAPTER));
        assert!(!adapter.get_property::<bool>("Discovering").await.unwrap());
        assert_eq!(
            error_name(adapter.call_method("StopDiscovery", &()).await),
            "org.bluez.Error.Failed"
        );
        assert!(!bluez.advertise(RANDOM, &[0x07]).await.unwrap());

        bluez.set_discovery_busy(ADAPTER, false);
        assert_eq!(bluez.busy_refusals(ADAPTER), 0);
        adapter.call_method("StartDiscovery", &()).await.unwrap();
        assert!(bluez.is_discovering(ADAPTER));
    }

    /// Discovery refused as busy is asked for again, once more at once and
    /// then after a while, and runs once the adapter takes it. Regression
    /// (#123): the scanner took `InProgress` for discovery it had already,
    /// and never asked again.
    #[tokio::test]
    async fn the_scanner_asks_again_for_discovery_refused_as_busy() {
        let (bus, bluez) = served().await;
        bluez.set_discovery_busy(ADAPTER, true);
        let mut scanning = Scanner::with_connection(bus.connect().await.unwrap())
            .scan()
            .await
            .unwrap();
        assert_eq!(
            bluez.busy_refusals(ADAPTER),
            2,
            "the start, and the one after a stop"
        );
        assert!(!bluez.is_discovering(ADAPTER));

        bluez.set_discovery_busy(ADAPTER, false);
        let scanning = tokio::spawn(async move { while scanning.next().await.is_some() {} });
        eventually(|| bluez.is_discovering(ADAPTER)).await;
        scanning.abort();
    }

    /// Discovery the scanner's connection has already is kept: `InProgress`
    /// to its start, it is stopped and started again, and runs.
    #[tokio::test]
    async fn the_scanner_keeps_discovery_it_has_already() {
        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        proxy(&conn, &adapter_path(ADAPTER), "org.bluez.Adapter1")
            .await
            .call_method("StartDiscovery", &())
            .await
            .unwrap();

        let _scanning = Scanner::with_connection(conn).scan().await.unwrap();
        assert!(bluez.is_discovering(ADAPTER));
    }

    /// With no adapter, or none powered, nothing scans, and the scanner says
    /// so.
    #[tokio::test]
    async fn the_scanner_fails_with_no_adapter_to_scan_on() {
        let bus = PrivateBus::start().unwrap();
        let bluez = FakeBluez::serve(bus.connect().await.unwrap())
            .await
            .unwrap();
        bluez.publish().await.unwrap();
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let err = scanner.start_discovery().await.unwrap_err();
        assert_eq!(err.to_string(), "BlueZ has no Bluetooth adapter");

        bluez.add_adapter(ADAPTER, false).await.unwrap();
        let err = scanner.start_discovery().await.unwrap_err();
        assert_eq!(err.to_string(), "no Bluetooth adapter is scanning");
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
            assert_eq!(event.device_path, device_path(ADAPTER, PAIR).as_str());
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
        let path = device_path(ADAPTER, PAIR).to_string();

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

        let path = device_path(ADAPTER, RANDOM);
        let adapter = adapter_path(ADAPTER);
        let mut device_signals = Vec::new();
        while device_signals.len() < 3 {
            let msg = timeout(TIMEOUT, signals.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if msg.header().path().is_some_and(|p| p.as_str() != adapter) {
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
        assert_eq!(
            OwnedObjectPath::try_from(device["Adapter"].clone())
                .unwrap()
                .as_str(),
            adapter
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

    /// Every adapter that discovers hears an advertisement, and makes a device
    /// of its own of the address; one that does not hears nothing. A paired
    /// device stays on its own adapter.
    #[tokio::test]
    async fn every_adapter_that_discovers_hears_every_advertisement() {
        let (bus, bluez) = served().await;
        bluez.add_adapter(SECOND, true).await.unwrap();
        bluez.add_adapter("hci2", false).await.unwrap();
        let conn = bus.connect().await.unwrap();
        let scanner = Scanner::with_connection(conn.clone());
        scanner
            .start_discovery_on(&adapter_path(SECOND))
            .await
            .unwrap();

        let devices = Devices::new(conn);
        let heard = |adapter, address| {
            let path = device_path(adapter, address);
            let devices = &devices;
            async move {
                let (address, on) = devices
                    .device_address_and_adapter(path.as_str())
                    .await
                    .ok()?;
                Some((address, on.ok()?))
            }
        };
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        assert_eq!(
            heard(SECOND, RANDOM).await,
            Some((RANDOM, adapter_path(SECOND)))
        );
        assert_eq!(heard(ADAPTER, RANDOM).await, None, "the first not scanning");

        scanner
            .start_discovery_on(&adapter_path(ADAPTER))
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        assert_eq!(
            heard(ADAPTER, RANDOM).await,
            Some((RANDOM, adapter_path(ADAPTER)))
        );
        assert_eq!(bluez.advertisers_on(SECOND), HashSet::from([RANDOM]));
        assert!(bluez.advertisers_on("hci2").is_empty(), "heard while off");
        assert_eq!(heard(SECOND, PAIR).await, None, "paired with the first");
    }

    /// The scanner takes what the fake signals: the plumbing the pipeline
    /// tests (#73) build on.
    #[tokio::test]
    async fn the_scanner_reads_its_advertisements() {
        let (bus, bluez) = served().await;
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        scanner.start_discovery().await.unwrap();

        // Twice from the one address, once as the object appears and once as a
        // change: the scanner reads both, in order, or the second alone when
        // its reader is behind.
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
        let path = adapter_path(ADAPTER);
        let adapter_b = proxy(&b, &path, "org.bluez.Adapter1").await;
        {
            let adapter_a = proxy(&a, &path, "org.bluez.Adapter1").await;
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
        assert!(bluez.is_discovering(ADAPTER));
        assert!(adapter_b.get_property::<bool>("Discovering").await.unwrap());

        drop(a);
        eventually(|| !bluez.is_discovering(ADAPTER)).await;
        assert!(!adapter_b.get_property::<bool>("Discovering").await.unwrap());
        assert!(!bluez.advertise(RANDOM, &[0x07]).await.unwrap());
    }

    /// The provider's batteries reach `Battery1` on the device, where GNOME
    /// Settings reads them, and follow every change.
    #[tokio::test]
    async fn reads_batteries_from_the_provider() {
        let (bus, bluez) = served().await;
        let mut provider =
            BatteryProvider::register(bus.connect().await.unwrap(), &adapter_path(ADAPTER))
                .await
                .unwrap();
        let reader = bus.connect().await.unwrap();
        let path = device_path(ADAPTER, PAIR).to_string();
        let battery = proxy(&reader, &path, "org.bluez.Battery1").await;

        // No such device: ignored, as bluetoothd ignores it.
        let nobody = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x99]);
        let actions = [
            BatteryAction::Add {
                mac: nobody,
                device_path: device_path(ADAPTER, nobody).to_string(),
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
        eventually(|| {
            bluez.ignored_batteries() == [(ADAPTER.to_string(), device_path(ADAPTER, nobody))]
        })
        .await;

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

    /// A provider's changes are taken in the order it made them, however
    /// fast they come: a battery added, withdrawn and added again at once is
    /// there at the end, at its last level. Regression: added, withdrawn and
    /// added each came on a stream of its own, and taken out of order, the
    /// second offer was refused as provided already, the withdrawal after it
    /// left no battery.
    #[tokio::test]
    async fn takes_a_providers_changes_in_order() {
        let (bus, bluez) = served().await;
        let mut provider =
            BatteryProvider::register(bus.connect().await.unwrap(), &adapter_path(ADAPTER))
                .await
                .unwrap();
        let add = |percentage| BatteryAction::Add {
            mac: PAIR,
            device_path: device_path(ADAPTER, PAIR).to_string(),
            percentage,
        };
        // Registering returns before the fake reads the provider, as with
        // bluetoothd: until the first battery shows, what follows could fall
        // ahead of the read, and only how it ended be seen.
        provider.apply(&add(73)).await.unwrap();
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 73)])).await;
        for round in 0..20 {
            provider
                .apply(&BatteryAction::Remove { mac: PAIR })
                .await
                .unwrap();
            let last = round == 19;
            provider
                .apply(&add(if last { 60 } else { 73 }))
                .await
                .unwrap();
        }
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 60)])).await;
        assert_eq!(bluez.withdrawn_batteries(), [PAIR; 20]);
    }

    /// A manager takes a battery only for a paired device on its own adapter,
    /// as bluetoothd's does: one on another adapter, and an advertiser, are
    /// ignored.
    #[tokio::test]
    async fn a_manager_takes_batteries_of_its_own_adapters_devices() {
        let (bus, bluez) = served().await;
        bluez.add_adapter(SECOND, true).await.unwrap();
        bluez.add_device(SECOND, OTHER, ALIAS).await.unwrap();
        Scanner::with_connection(bus.connect().await.unwrap())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());

        let conn = bus.connect().await.unwrap();
        let mut first = BatteryProvider::register(conn.clone(), &adapter_path(ADAPTER))
            .await
            .unwrap();
        let add = |mac, adapter| BatteryAction::Add {
            mac,
            device_path: device_path(adapter, mac).to_string(),
            percentage: 40,
        };
        for action in [add(OTHER, SECOND), add(RANDOM, ADAPTER), add(PAIR, ADAPTER)] {
            first.apply(&action).await.unwrap();
        }
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 40)])).await;
        // In whichever order they were read.
        let ignored = || {
            let mut ignored = bluez.ignored_batteries();
            ignored.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));
            ignored
        };
        eventually(|| {
            ignored()
                == [
                    (ADAPTER.to_string(), device_path(ADAPTER, RANDOM)),
                    (ADAPTER.to_string(), device_path(SECOND, OTHER)),
                ]
        })
        .await;

        let mut second = BatteryProvider::register(conn, &adapter_path(SECOND))
            .await
            .unwrap();
        second.apply(&add(OTHER, SECOND)).await.unwrap();
        eventually(|| bluez.batteries_on(SECOND) == HashMap::from([(OTHER, 40)])).await;
        assert_eq!(bluez.batteries_on(ADAPTER), HashMap::from([(PAIR, 40)]));
    }

    /// A provider that leaves the bus takes its batteries along.
    #[tokio::test]
    async fn a_provider_leaving_takes_its_batteries() {
        let (bus, bluez) = served().await;
        let mut provider =
            BatteryProvider::register(bus.connect().await.unwrap(), &adapter_path(ADAPTER))
                .await
                .unwrap();
        let add = BatteryAction::Add {
            mac: PAIR,
            device_path: device_path(ADAPTER, PAIR).to_string(),
            percentage: 73,
        };
        provider.apply(&add).await.unwrap();
        eventually(|| !bluez.batteries().is_empty()).await;

        drop(provider);
        eventually(|| bluez.batteries().is_empty()).await;
    }

    /// Powered off, an adapter stops as bluetoothd's does: discovery there
    /// ends, what it only heard advertising goes, its connected devices
    /// disconnect, and nothing connects until it is on again. Its pairings and
    /// providers stay. Each change is signalled.
    #[tokio::test]
    async fn powering_off_stops_the_adapter() {
        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        let devices = Devices::new(conn.clone());
        let events = devices.watch_connections().await.unwrap();
        tokio::pin!(events);
        let path = adapter_path(ADAPTER);
        let adapter = proxy(&conn, &path, "org.bluez.Adapter1").await;
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .unwrap()
            .member("PropertiesChanged")
            .unwrap()
            .path(path.as_str())
            .unwrap()
            .build();
        let powered = MessageStream::for_match_rule(rule, &conn, None)
            .await
            .unwrap()
            .filter_map(|msg| async move {
                let (_, changed, _) = msg
                    .ok()?
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    .ok()?;
                bool::try_from(changed.get("Powered")?.clone()).ok()
            });
        tokio::pin!(powered);
        let _provider = BatteryProvider::register(conn.clone(), &path)
            .await
            .unwrap();
        Scanner::with_connection(conn.clone())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        bluez.set_connected(PAIR, true).await.unwrap();

        bluez.set_powered(ADAPTER, false).await.unwrap();
        bluez.set_powered(ADAPTER, false).await.unwrap();
        assert!(!adapter.get_property::<bool>("Powered").await.unwrap());
        assert!(!bluez.is_discovering(ADAPTER));
        assert!(!adapter.get_property::<bool>("Discovering").await.unwrap());
        assert!(bluez.advertisers_on(ADAPTER).is_empty());
        assert!(
            !devices
                .device_connected(device_path(ADAPTER, RANDOM).as_str())
                .await
        );
        let pair = device_path(ADAPTER, PAIR).to_string();
        assert!(!devices.device_connected(&pair).await);
        assert!(bluez.set_connected(PAIR, true).await.is_err(), "while off");
        assert!(!bluez.can_connect(PAIR));
        for connected in [true, false] {
            let event = timeout(TIMEOUT, events.next()).await.unwrap().unwrap();
            assert_eq!(
                (event.device_path.as_str(), event.connected),
                (pair.as_str(), Some(connected))
            );
        }
        assert_eq!(bluez.providers_on(ADAPTER), 1, "kept while off");

        bluez.set_powered(ADAPTER, true).await.unwrap();
        assert!(adapter.get_property::<bool>("Powered").await.unwrap());
        assert!(!bluez.is_discovering(ADAPTER), "not by itself");
        assert!(!devices.device_connected(&pair).await, "not by itself");
        bluez.set_connected(PAIR, true).await.unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            seen.push(timeout(TIMEOUT, powered.next()).await.unwrap().unwrap());
        }
        assert_eq!(seen, [false, true], "each change once");
        assert!(bluez.set_powered("hci9", true).await.is_err(), "no adapter");
    }

    /// An adapter is plugged in and out at any time, each signalled. Out, it
    /// takes its devices, providers and batteries along and leaves the other
    /// adapter alone; in again, it is the same controller, with its pairs back,
    /// disconnected.
    #[tokio::test]
    async fn adapters_are_plugged_in_and_out() {
        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        let om = zbus::fdo::ObjectManagerProxy::builder(&conn)
            .destination(BLUEZ_SERVICE)
            .unwrap()
            .path("/")
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut added = om.receive_interfaces_added().await.unwrap();
        let mut removed = om.receive_interfaces_removed().await.unwrap();

        bluez.add_adapter(SECOND, true).await.unwrap();
        let second = adapter_path(SECOND);
        // The manager comes ahead of the adapter.
        let mut announced = Vec::new();
        while !announced.iter().any(|i| i == "org.bluez.Adapter1") {
            let signal = timeout(TIMEOUT, added.next()).await.unwrap().unwrap();
            let args = signal.args().unwrap();
            assert_eq!(args.object_path().as_str(), second);
            announced.extend(
                args.interfaces_and_properties()
                    .keys()
                    .map(|i| i.to_string()),
            );
        }
        assert_eq!(
            announced,
            ["org.bluez.BatteryProviderManager1", "org.bluez.Adapter1"]
        );
        let address = proxy(&conn, &second, "org.bluez.Adapter1")
            .await
            .get_property::<String>("Address")
            .await
            .unwrap();
        bluez.add_device(SECOND, OTHER, ALIAS).await.unwrap();
        bluez.set_connected(OTHER, true).await.unwrap();
        let mut provider = BatteryProvider::register(conn.clone(), &second)
            .await
            .unwrap();
        let first = BatteryProvider::register(conn.clone(), &adapter_path(ADAPTER))
            .await
            .unwrap();
        let add = |mac, adapter| BatteryAction::Add {
            mac,
            device_path: device_path(adapter, mac).to_string(),
            percentage: 40,
        };
        provider.apply(&add(OTHER, SECOND)).await.unwrap();
        let mut first = first;
        first.apply(&add(PAIR, ADAPTER)).await.unwrap();
        eventually(|| bluez.batteries().len() == 2).await;

        bluez.remove_adapter(SECOND).await.unwrap();
        let found = bluez::adapters(&conn).await.unwrap();
        assert_eq!(
            found.iter().map(|a| a.path.as_str()).collect::<Vec<_>>(),
            [adapter_path(ADAPTER)]
        );
        assert_eq!(bluez.providers_on(SECOND), 0);
        assert_eq!(
            bluez.batteries(),
            HashMap::from([(PAIR, 40)]),
            "the other's stays"
        );
        let devices = Devices::new(conn.clone());
        assert!(
            devices
                .device_address_and_adapter(device_path(SECOND, OTHER).as_str())
                .await
                .is_err()
        );
        assert!(
            bluez.set_connected(OTHER, true).await.is_err(),
            "gone with it"
        );
        let mut gone = Vec::new();
        while !gone.iter().any(|i| i == "org.bluez.Adapter1") {
            let signal = timeout(TIMEOUT, removed.next()).await.unwrap().unwrap();
            let args = signal.args().unwrap();
            if args.object_path().as_str() == second {
                gone.extend(args.interfaces().iter().map(|i| i.to_string()));
            }
        }
        assert!(bluez.remove_adapter(SECOND).await.is_err(), "out already");

        bluez.add_adapter(SECOND, false).await.unwrap();
        let again = proxy(&conn, &second, "org.bluez.Adapter1").await;
        assert_eq!(
            again.get_property::<String>("Address").await.unwrap(),
            address
        );
        let other = device_path(SECOND, OTHER).to_string();
        assert_eq!(
            devices.device_address_and_adapter(&other).await.unwrap().0,
            OTHER
        );
        assert!(!devices.device_connected(&other).await);
        assert_eq!(devices.device_alias(&other).await, ALIAS);
    }

    /// Restarted, the fake BlueZ lets go of its name and takes it again, and
    /// forgets its clients' discovery and providers, and what it only heard
    /// advertising. The adapters and connections are as they were.
    #[tokio::test]
    async fn a_restart_forgets_every_client() {
        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        let mut owners = dbus
            .receive_name_owner_changed_with_args(&[(0, BLUEZ_SERVICE)])
            .await
            .unwrap();
        let mut provider = BatteryProvider::register(conn.clone(), &adapter_path(ADAPTER))
            .await
            .unwrap();
        Scanner::with_connection(conn.clone())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        bluez.set_connected(PAIR, true).await.unwrap();
        let add = BatteryAction::Add {
            mac: PAIR,
            device_path: device_path(ADAPTER, PAIR).to_string(),
            percentage: 73,
        };
        provider.apply(&add).await.unwrap();
        eventually(|| !bluez.batteries().is_empty()).await;

        let old = bluez.connection().unique_name().unwrap().to_string();
        bluez.restart(bus.connect().await.unwrap()).await.unwrap();
        let new = bluez.connection().unique_name().unwrap().to_string();
        assert_ne!(new, old, "the same bluetoothd");
        for (from, to) in [(Some(&old), None), (None, Some(&new))] {
            let signal = timeout(TIMEOUT, owners.next()).await.unwrap().unwrap();
            let args = signal.args().unwrap();
            let named =
                |name: &Option<zbus::names::UniqueName<'_>>| name.as_ref().map(|n| n.to_string());
            assert_eq!(named(args.old_owner()).as_ref(), from);
            assert_eq!(named(args.new_owner()).as_ref(), to);
        }
        assert!(!bluez.is_discovering(ADAPTER));
        assert!(bluez.advertisers_on(ADAPTER).is_empty());
        assert_eq!(bluez.providers_on(ADAPTER), 0);
        assert!(bluez.batteries().is_empty());
        let devices = Devices::new(conn.clone());
        assert!(
            devices
                .device_connected(device_path(ADAPTER, PAIR).as_str())
                .await
        );
        assert!(bluez.is_powered(ADAPTER));
        assert!(
            !bluez.advertise(RANDOM, &[0x07]).await.unwrap(),
            "nobody discovers"
        );
        assert_eq!(
            devices
                .device_alias(device_path(ADAPTER, PAIR).as_str())
                .await,
            ALIAS
        );
        // Back from `register` either way; only the new one holding it says
        // it registered anew.
        BatteryProvider::register(conn, &adapter_path(ADAPTER))
            .await
            .unwrap();
        assert_eq!(
            bluez.providers_on(ADAPTER),
            1,
            "registered with the new one"
        );
    }

    /// On a bus that activates `org.bluez`, as systemd's does, a call made
    /// while the fake BlueZ is away waits for the new one, and is answered by
    /// it: the old one is never asked.
    #[tokio::test]
    async fn a_call_while_away_waits_for_the_new_one_where_bluez_is_activated() {
        let bus = PrivateBus::activating(BLUEZ_SERVICE).unwrap();
        let bluez = FakeBluez::serve(bus.connect().await.unwrap())
            .await
            .unwrap();
        bluez.add_adapter(ADAPTER, true).await.unwrap();
        bluez.publish().await.unwrap();
        assert_eq!(bus.activations(), 0, "owned already");

        bluez.stop().await.unwrap();
        let client = bus.connect().await.unwrap();
        let asked = tokio::spawn(async move { bluez::adapters(&client).await });
        eventually(|| bus.activations() == 1).await;
        assert!(!asked.is_finished(), "answered while away");
        bluez.start(bus.connect().await.unwrap()).await.unwrap();
        let adapters = timeout(TIMEOUT, asked).await.unwrap().unwrap().unwrap();
        assert_eq!(adapters.len(), 1);
    }

    /// Without the battery API there is no manager: registering fails as the
    /// method is unknown, and the adapter is there all the same.
    #[tokio::test]
    async fn an_adapter_can_lack_the_battery_api() {
        let (bus, bluez) = served().await;
        bluez.drop_battery_api(ADAPTER).await.unwrap();
        let conn = bus.connect().await.unwrap();
        let refused = BatteryProvider::register(conn.clone(), &adapter_path(ADAPTER)).await;
        assert!(refused.is_err());
        assert_eq!(bluez::adapters(&conn).await.unwrap().len(), 1);
        bluez.remove_adapter(ADAPTER).await.unwrap();
    }

    /// A root registers once with a manager, whoever registers it, and only
    /// its owner unregisters it. Each adapter's manager keeps its own.
    #[tokio::test]
    async fn a_provider_registers_once_with_each_adapter() {
        let (bus, bluez) = served().await;
        bluez.add_adapter(SECOND, true).await.unwrap();
        let (conn, other) = (bus.connect().await.unwrap(), bus.connect().await.unwrap());
        let first = adapter_path(ADAPTER);
        let manager = proxy(&conn, &first, "org.bluez.BatteryProviderManager1").await;
        let root = ObjectPath::try_from("/provider").unwrap();
        let elsewhere = ObjectPath::try_from("/other").unwrap();

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
        let theirs = proxy(&other, &first, "org.bluez.BatteryProviderManager1").await;
        assert_eq!(
            error_name(
                theirs
                    .call_method("RegisterBatteryProvider", &(&root,))
                    .await
            ),
            "org.bluez.Error.AlreadyExists"
        );
        assert_eq!(
            error_name(
                theirs
                    .call_method("UnregisterBatteryProvider", &(&root,))
                    .await
            ),
            "org.bluez.Error.DoesNotExist"
        );
        assert_eq!(
            error_name(
                manager
                    .call_method("UnregisterBatteryProvider", &(&elsewhere,))
                    .await
            ),
            "org.bluez.Error.DoesNotExist"
        );

        let second = adapter_path(SECOND);
        proxy(&conn, &second, "org.bluez.BatteryProviderManager1")
            .await
            .call_method("RegisterBatteryProvider", &(&root,))
            .await
            .expect("a manager of its own");
        manager
            .call_method("UnregisterBatteryProvider", &(&root,))
            .await
            .unwrap();
    }
}
