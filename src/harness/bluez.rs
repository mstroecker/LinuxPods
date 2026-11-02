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
//!   its real address: `Address`, `Adapter`, `Alias`, `Connected`,
//!   `ServicesResolved`, `Paired` and `UUIDs`, the AirPods service among them
//!   unless [`FakeBluez::set_airpods_service`] took it out
//! - a `Device1` for each random address an adapter heard an advertisement
//!   from, under that adapter, with its `ManufacturerData`
//! - `Battery1` on a device once a registered provider gives it a level - the
//!   battery GNOME Settings shows
//! - `MediaTransport1` under a connected device while audio is set up with
//!   it - see [Audio](#audio)
//!
//! Nothing else of bluetoothd is here - no pairing, no `Device1.Connect`, no
//! agent - since the app uses none of it.
//!
//! # Adapters coming and going
//!
//! What bluetoothd does to its clients when an adapter or bluetoothd itself
//! goes, which the app has to recover from (#50):
//!
//! - [`FakeBluez::set_powered`] switches an adapter on or off, signaling
//!   `Powered`. Off, it does what bluetoothd's `adapter_stop` does: every
//!   client's discovery there ends, with `Discovering` signaled; the devices
//!   it only heard advertising go, as when discovery ends (see
//!   [Advertisements](#advertisements)); and its connected devices
//!   disconnect. Its paired devices and the providers registered with it
//!   stay, as they do in bluetoothd. Nothing connects to a device on an
//!   adapter that is off.
//!
//!   bluetoothd's order differs: it only queues `Powered` (`settings_changed`
//!   in `src/adapter.c`), and the D-Bus layer sends queued changes later,
//!   while a device's removal goes out at once. So on hardware the devices
//!   go first, and `Powered` follows with `Discovering`, whichever way the
//!   adapter powers off. The two ways differ only in the `RSSI`: powered off
//!   from below bluetoothd - rfkill, as the desktop's Bluetooth switch does
//!   it, or the kernel - `adapter_stop`'s `discovery_cleanup` invalidates it
//!   on each device before it goes; powered off over D-Bus, bluetoothd
//!   removes its temporary devices before it asks the kernel, with none
//!   invalidated (`property_set_mode`). The fake signals `Powered` first, then
//!   stops the adapter as rfkill has it stopped; it keeps that order until
//!   something needs bluetoothd's.
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
//! Signaled as bluetoothd signals them in the #71 captures, which agree with
//! its source (`btd_adapter_device_found` in `src/adapter.c`):
//!
//! - An address an adapter hears for the first time becomes a device object
//!   under that adapter, announced by `InterfacesAdded` with its properties,
//!   `ManufacturerData` and `RSSI` among them. bluetoothd fills the object in
//!   before it announces it, so no `PropertiesChanged` follows for that first
//!   advertisement.
//! - Each later advertisement from the address is one `PropertiesChanged` on
//!   `org.bluez.Device1`: `RSSI` every time, changed or not, and
//!   `ManufacturerData` in the same signal only when its bytes changed. Real
//!   AirPods repeat a payload for hundreds of advertisements, so most of
//!   what they bring is `RSSI` alone - 95% of the signals in the first
//!   capture - and every one of them reaches whoever subscribes to
//!   `PropertiesChanged`, attachment's connection watch among them.
//! - An address not heard for [`TEMPORARY_TIMEOUT`] goes with
//!   `InterfacesRemoved`: 29.8-30.7 s after its last advertisement in the
//!   captures, and an address heard again after that is announced afresh.
//! - When the last client discovering on an adapter stops, or leaves the bus,
//!   the devices it only heard advertising go at once, as bluetoothd's
//!   `discovery_cleanup` removes those that are temporary, not connectable
//!   and not connected: device by device, its `RSSI` invalidated and then its
//!   `InterfacesRemoved`, and `Discovering` after all of them. The pods'
//!   proximity advertisements are not connectable, and on 2026-11-02 their
//!   devices were gone right after the app quit with nobody else
//!   discovering (#153). Every device the fake makes of an advertisement is
//!   temporary, not connectable and not connected, so every one goes, and
//!   none stays without its `RSSI`; a paired device is never one. So whoever
//!   starts discovering next hears each address afresh, in an
//!   `InterfacesAdded` with its payload, at its next advertisement. While
//!   another client discovers, the devices stay, and a client that starts
//!   then hears an address only once its payload changes, or a new one
//!   comes.
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
//! `PropertiesChanged`. Stricter than bluetoothd, it takes none of those
//! signals until its `GetManagedObjects` is answered, where bluetoothd takes
//! `InterfacesAdded` regardless: a provider that leaves the read unanswered
//! gets no battery here at all, which is how a dropped read showed (#125).
//! A battery takes its device from `Device` when it
//! appears. It is ignored, with a warning, if that is no paired device on the
//! provider's adapter - one on another adapter, an advertiser, or nothing at
//! all - or if another provider got there first. A client that leaves the bus
//! takes its providers' batteries along, and its discovery stops.
//!
//! # Audio
//!
//! The transports bluetoothd makes as the audio server - PipeWire - sets
//! audio up with a connected device, which the app lowers the volume on while
//! the user speaks (#150). No audio server is here: [`FakeBluez::set_playing`],
//! [`FakeBluez::set_call`] and [`FakeBluez::set_volume`] do what it, or the
//! user, would.
//!
//! - Playing, a device has an A2DP transport, `org.bluez.MediaTransport1`
//!   with `Device`, `UUID` (the host's audio source), `Codec`, `State` and
//!   `Volume`, at `<device>/sep1/fd<n>`, `n` new to the device with every
//!   transport, so a transport set up again is at a new path, as a real one
//!   is. bluetoothd numbers them otherwise - the first number free from the
//!   count of all its transports (`media_transport_create` in BlueZ 5.87's
//!   `profiles/audio/transport.c`) - which nothing here depends on. It
//!   starts `active`, at the device's own volume, which outlasts it as
//!   bluetoothd keeps it on the device (`btd_device_set_volume`); stopped, it
//!   goes `idle` and stays.
//! - `Volume` is AVRCP's absolute volume, 0 to 127, writable by any client,
//!   as bluetoothd checks no owner for it (`transport.c`). Each client's set
//!   is recorded ([`FakeBluez::volume_sets`]); the user's are not. A set of
//!   the volume the transport holds does nothing, as in bluetoothd. Any
//!   other is taken at once and signaled - unless the pair is made to answer
//!   as bluetoothd has a real pair answer ([`FakeBluez::set_volume_answer`]):
//!   bluetoothd sends the volume to the pods over AVRCP and changes the
//!   property only once they answer, to the volume they report (BlueZ 5.87's
//!   `transport.c` and `profiles/audio/avrcp.c`). The fake then changes it
//!   after a delay, and may round it to an even volume, as a pair might. How
//!   long real pods take, and whether they round, is untried (#150).
//! - A call takes A2DP to `idle`, and after it A2DP plays again if it did.
//!   That is how PipeWire's own HFP backend is assumed to show it - not read
//!   in its source, nor watched (#150): no transport of its own in BlueZ, and
//!   A2DP released while the pair is on the headset profile. Whether A2DP
//!   stays, idle, or goes is a hardware check.
//! - A disconnect takes the device's transports along, and so does a restart:
//!   a new bluetoothd has none until the audio server sets A2DP up again.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::StreamExt;
use tokio::task::AbortHandle;
use zbus::message::Header;
use zbus::names::{BusName, InterfaceName, OwnedUniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, interface};

use crate::ble::scanner::APPLE_COMPANY_ID;
use crate::bluez::{AIRPODS_UUID, BLUEZ_SERVICE};
use crate::harness::connect_serving;
use crate::volume::MAX_VOLUME;

/// How long bluetoothd keeps a device it has only heard advertise once it
/// stops hearing it: `TemporaryTimeout` in its main.conf, 30 seconds unless
/// set otherwise.
pub const TEMPORARY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long after `Connected` a paired device's `ServicesResolved` follows,
/// by default: 4.1 s and 4.5 s in the #71 captures. The real value, as
/// nothing in the app waits on it - see [`FakeBluez::set_services_delay`].
pub const SERVICES_RESOLVED: Duration = Duration::from_secs(4);

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

/// The signal strength every advertisement arrives at. Nothing reads it, but
/// every advertisement signals it.
const RSSI: i16 = -60;

const DEVICE_INTERFACE: &str = "org.bluez.Device1";

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
    /// Held while a restart carries the paired devices over to its new
    /// connection, and while a device's services are resolved: so a
    /// resolve lands either on the object carried over before it is read, or
    /// on its copy, never on one left behind.
    carrying: Arc<tokio::sync::Mutex<()>>,
    /// Held while devices only heard advertising are made, changed or
    /// removed: so a device a discovery's end removes cannot be one an
    /// advertisement is bringing up or changing meanwhile.
    hearing: Arc<tokio::sync::Mutex<()>>,
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
    /// How long after a connect services are resolved; `None` for
    /// [`SERVICES_RESOLVED`].
    services_delay: Option<Duration>,
    /// The audio transports of each connected device that has any - see
    /// [Audio](#audio).
    audio: HashMap<Address, Audio>,
    /// Every volume a client set on a transport, by device, with when.
    volume_sets: HashMap<Address, Vec<(Instant, u16)>>,
}

/// A device's audio transports - see [Audio](#audio).
#[derive(Default)]
struct Audio {
    /// The A2DP transport, while the audio server has A2DP set up.
    a2dp: Option<OwnedObjectPath>,
    /// Whether a call has the pair on the headset profile.
    in_call: bool,
    /// Whether A2DP played when the call came, to play again after it.
    resume: bool,
}

/// A paired device's pairing: the adapter it is with, and its name.
struct Pairing {
    adapter: String,
    alias: String,
    /// How many times it has connected, so that services resolved late are
    /// resolved for the connection they were for. Kept here, as the
    /// pairing is, through its object going and coming with its adapter
    /// and through a restart.
    connections: u64,
    /// The device's own volume, AVRCP's absolute volume: what its A2DP
    /// transport starts at, kept from one to the next as bluetoothd keeps it
    /// on the device.
    volume: u16,
    /// How many transports it has had, which numbers the next one's path:
    /// `fd0`, `fd1` and on - see [Audio](#audio).
    transports: u32,
    /// How it answers a client's volume - see
    /// [`FakeBluez::set_volume_answer`].
    answer: VolumeAnswer,
}

/// How a pair answers a volume a client sets, as bluetoothd has it
/// answer - see [Audio](#audio). By default at once, as set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeAnswer {
    /// How long after the set the transport reads the new volume.
    pub delay: Duration,
    /// Whether the pair rounds the volume down to an even one.
    pub to_even: bool,
}

impl VolumeAnswer {
    /// The volume the pair answers `volume` with.
    fn answered(self, volume: u16) -> u16 {
        if self.to_even { volume & !1 } else { volume }
    }
}

/// The volume a pair's first transport starts at: half way.
pub const DEFAULT_VOLUME: u16 = 64;

/// The A2DP transport's `UUID`: the host's audio source, the end of
/// headphones bluetoothd makes the transport for.
pub const A2DP_SOURCE_UUID: &str = "0000110a-0000-1000-8000-00805f9b34fb";

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
    /// Serves the ObjectManager on a connection of its own to the bus at
    /// `bus`, with no adapter yet - see [`FakeBluez::add_adapter`]. The name
    /// comes later, from [`FakeBluez::publish`], so that adapters and devices
    /// can be put in place before the app can find any of it.
    pub async fn serve(bus: &str) -> Result<Self> {
        let conn = connect_serving(bus).await?;
        let bluez = Self {
            conn: Arc::new(Mutex::new(conn.clone())),
            state: Arc::default(),
            exports: Arc::default(),
            carrying: Arc::default(),
            hearing: Arc::default(),
        };
        bluez.serve_on(&conn).await?;
        Ok(bluez)
    }

    /// What every connection it serves on has besides the ObjectManager at
    /// `/`, which [`connect_serving`] serves from the start: a watch on
    /// clients leaving the bus.
    async fn serve_on(&self, conn: &Connection) -> Result<()> {
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
    /// Signaled with `InterfacesAdded` once the name is taken, as a USB
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

    /// Powers the adapter called `name` on or off, signaling `Powered`;
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
        // After `Powered`, where bluetoothd's queued `Powered` comes after the
        // removals - see the module docs.
        if !powered {
            self.stop_adapter(name).await;
        }
        Ok(())
    }

    /// What bluetoothd's `adapter_stop` does to an adapter's clients: their
    /// discovery there ends, the devices it only heard advertising go as at
    /// the end of a discovery - see [`FakeBluez::remove_advertisers`] - and
    /// its connected devices disconnect, each signaled.
    async fn stop_adapter(&self, name: &str) {
        let hearing = self.hearing.lock().await;
        let (ended, heard, paired) = {
            let mut state = self.state();
            let ended = state.end_discovery(|adapter| adapter == name);
            let heard = state.forget_advertisers(|adapter| adapter == name);
            (ended, heard, state.paired_with(name))
        };
        self.remove_advertisers(heard).await;
        drop(hearing);
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
    /// [`FakeBluez::start`] on the bus at `bus`. Back from here, `org.bluez`
    /// is taken again, under a new unique name.
    pub async fn restart(&self, bus: &str) -> Result<()> {
        self.stop().await?;
        self.start(bus).await
    }

    /// bluetoothd going, as it does when it crashes: every client's
    /// discovery and providers, and the devices only heard advertising, are
    /// forgotten, and `org.bluez` let go of. Until [`FakeBluez::start`], a
    /// call to `org.bluez` finds no one - or, on a bus that activates it,
    /// waits for the new one.
    pub async fn stop(&self) -> Result<()> {
        let hearing = self.hearing.lock().await;
        let (heard, batteries, transports) = {
            let mut state = self.state();
            state.end_discovery(|_| true);
            let heard = state.forget_advertisers(|_| true);
            let transports: Vec<OwnedObjectPath> = state
                .audio
                .drain()
                .filter_map(|(_, audio)| audio.a2dp)
                .collect();
            (heard, state.forget_providers(|_| true), transports)
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
        // The audio server's transports too: a new bluetoothd has none until
        // the audio server sets A2DP up again.
        for path in transports {
            let _ = server.remove::<MediaTransport1, _>(&path).await;
        }
        drop(hearing);
        conn.release_name(BLUEZ_SERVICE)
            .await
            .context("failed to let go of org.bluez")?;
        Ok(())
    }

    /// The new bluetoothd, on a new connection to the bus at `bus`, which
    /// has a unique name of its own: the adapters and paired devices as the
    /// old one left them, connections and all, none discovering. All of it is
    /// in place before the name is taken, so nothing is announced to whoever
    /// listens to `org.bluez`.
    pub async fn start(&self, bus: &str) -> Result<()> {
        let conn = connect_serving(bus).await?;
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
        // As the old one left them, none resolving its services meanwhile.
        let carrying = self.carrying.lock().await;
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
                    services_resolved: device.services_resolved,
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
        drop(carrying);
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
                connections: 0,
                volume: DEFAULT_VOLUME,
                transports: 0,
                answer: VolumeAnswer::default(),
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
            services_resolved: false,
            airpods_service: true,
        };
        self.conn()
            .object_server()
            .at(device_path(adapter, address), device)
            .await?;
        Ok(())
    }

    /// Connects or disconnects a paired device. A change is signaled, as
    /// bluetoothd signals it; setting what is already set signals nothing.
    /// Nothing connects on an adapter that is off.
    ///
    /// As in the #71 captures, a connection has its services resolved a while
    /// after `Connected=true` - see [`FakeBluez::set_services_delay`] - and a
    /// disconnect signals `ServicesResolved=false` just before
    /// `Connected=false`, if it got that far.
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
    /// signaling a change, and its `ServicesResolved` as
    /// [`FakeBluez::set_connected`] says.
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
        if device.connected == connected {
            return Ok(());
        }
        let emitter = iface.signal_emitter();
        if !connected && device.services_resolved {
            device.services_resolved = false;
            device.services_resolved_changed(emitter).await?;
        }
        device.connected = connected;
        device.connected_changed(emitter).await?;
        // Its audio goes with the connection.
        if !connected {
            drop(device);
            self.drop_audio(address).await;
        }
        if connected {
            let (connection, delay) = {
                let mut state = self.state();
                let delay = state.services_delay.unwrap_or(SERVICES_RESOLVED);
                let pairing = state
                    .paired
                    .get_mut(&address)
                    .with_context(|| format!("no paired device {address}"))?;
                pairing.connections += 1;
                (pairing.connections, delay)
            };
            let bluez = self.clone();
            let adapter = adapter.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                bluez.resolve_services(&adapter, address, connection).await;
            });
        }
        Ok(())
    }

    /// Resolves a paired device's services, if it is still on the connection
    /// counted `connection`, signaling `ServicesResolved`.
    async fn resolve_services(&self, adapter: &str, address: Address, connection: u64) {
        let _carrying = self.carrying.lock().await;
        let current = self.state().paired.get(&address).map(|p| p.connections);
        if current != Some(connection) {
            return;
        }
        let path = device_path(adapter, address);
        let conn = self.conn();
        // Gone with its adapter in the meantime.
        let Ok(iface) = conn
            .object_server()
            .interface::<_, PairedDevice>(path)
            .await
        else {
            return;
        };
        let mut device = iface.get_mut().await;
        if !device.connected || device.services_resolved {
            return;
        }
        device.services_resolved = true;
        if let Err(e) = device
            .services_resolved_changed(iface.signal_emitter())
            .await
        {
            tracing::warn!("fake BlueZ: failed to resolve the services of {address}: {e}");
        }
    }

    /// Sets how long after a connect a paired device's services are resolved,
    /// for the connections from here on: [`SERVICES_RESOLVED`] unless set.
    pub fn set_services_delay(&self, delay: Duration) {
        self.state().services_delay = Some(delay);
    }

    /// Lists the AirPods service among a paired device's services, or takes it
    /// out, signaling the change as bluetoothd signals services found. Without
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

    /// Starts or stops audio to a connected paired device, as the audio
    /// server does - see [Audio](#audio). Playing, its A2DP transport is
    /// `active`: set up first if it has none, at a path new to it, at the
    /// device's own volume. Stopped, it is `idle`, as when the audio server
    /// releases it. During a call this says only whether A2DP plays again
    /// once the call is over. Fails for a device that is not connected.
    pub async fn set_playing(&self, address: Address, playing: bool) -> Result<()> {
        let device = self.connected_path(address).await?;
        let a2dp = {
            let mut state = self.state();
            let audio = state.audio.entry(address).or_default();
            if audio.in_call {
                audio.resume = playing;
                return Ok(());
            }
            audio.a2dp.clone()
        };
        match a2dp {
            Some(path) => self.set_transport_state(&path, playing).await,
            None if playing => {
                let path = self.add_transport(address, &device).await?;
                self.state().audio.entry(address).or_default().a2dp = Some(path);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Starts or ends a call on a connected paired device, as the audio
    /// server switching it to the headset profile and back is assumed to -
    /// see [Audio](#audio): its A2DP transport, if any, goes `idle`; after the
    /// call it plays again if it did before, at the volume it had.
    pub async fn set_call(&self, address: Address, on: bool) -> Result<()> {
        self.connected_path(address).await?;
        let a2dp = {
            let mut state = self.state();
            let audio = state.audio.entry(address).or_default();
            if on == audio.in_call {
                return Ok(());
            }
            audio.in_call = on;
            audio.a2dp.clone()
        };
        let Some(path) = a2dp else {
            return Ok(());
        };
        if on {
            let playing = self.transport_state(&path).await? == "active";
            self.state().audio.entry(address).or_default().resume = playing;
            return self.set_transport_state(&path, false).await;
        }
        let resume = std::mem::take(&mut self.state().audio.entry(address).or_default().resume);
        if resume {
            self.set_transport_state(&path, true).await?;
        }
        Ok(())
    }

    /// Sets a device's A2DP volume as the user does - GNOME's slider, keys,
    /// the pods' own controls - signaling the change. Not among
    /// [`FakeBluez::volume_sets`], which are the clients'. With no A2DP
    /// transport, it is the volume the next one starts at, as though the
    /// pair had come back at another volume. Fails for a device that is not
    /// paired, or above 127.
    pub async fn set_volume(&self, address: Address, volume: u16) -> Result<()> {
        anyhow::ensure!(volume <= MAX_VOLUME, "{volume} is above {MAX_VOLUME}");
        match self.a2dp_transport(address) {
            Some(path) => self.answer_volume(&path, address, volume).await,
            None => {
                let mut state = self.state();
                let pairing = state
                    .paired
                    .get_mut(&address)
                    .with_context(|| format!("no paired device {address}"))?;
                pairing.volume = volume;
                Ok(())
            }
        }
    }

    /// Has a paired device answer a volume a client sets as `answer` says -
    /// see [Audio](#audio).
    pub fn set_volume_answer(&self, address: Address, answer: VolumeAnswer) -> Result<()> {
        let mut state = self.state();
        let pairing = state
            .paired
            .get_mut(&address)
            .with_context(|| format!("no paired device {address}"))?;
        pairing.answer = answer;
        Ok(())
    }

    /// How a paired device answers a volume a client sets.
    pub fn volume_answer(&self, address: Address) -> Option<VolumeAnswer> {
        Some(self.state().paired.get(&address)?.answer)
    }

    /// Changes the volume of the transport at `path`, a device's, signaling
    /// it, and keeps it as the device's own. Nothing if the transport is gone.
    async fn answer_volume(
        &self,
        path: &OwnedObjectPath,
        address: Address,
        volume: u16,
    ) -> Result<()> {
        let Ok(iface) = self
            .conn()
            .object_server()
            .interface::<_, MediaTransport1>(path)
            .await
        else {
            return Ok(());
        };
        let mut transport = iface.get_mut().await;
        transport.volume = volume;
        transport.volume_changed(iface.signal_emitter()).await?;
        drop(transport);
        if let Some(pairing) = self.state().paired.get_mut(&address) {
            pairing.volume = volume;
        }
        Ok(())
    }

    /// The volume of a device's A2DP transport, `None` without one.
    pub async fn volume(&self, address: Address) -> Option<u16> {
        let path = self.a2dp_transport(address)?;
        let iface = self
            .conn()
            .object_server()
            .interface::<_, MediaTransport1>(path)
            .await
            .ok()?;
        Some(iface.get().await.volume)
    }

    /// The path of a device's A2DP transport, `None` without one.
    pub fn a2dp_transport(&self, address: Address) -> Option<OwnedObjectPath> {
        self.state().audio.get(&address)?.a2dp.clone()
    }

    /// Every volume a client set on a device's transports, in order, with
    /// when: the app's, never the user's.
    pub fn volume_sets(&self, address: Address) -> Vec<(Instant, u16)> {
        self.state()
            .volume_sets
            .get(&address)
            .cloned()
            .unwrap_or_default()
    }

    /// The path of a paired device that is connected.
    async fn connected_path(&self, address: Address) -> Result<OwnedObjectPath> {
        let path = self.state().paired_path(address)?;
        let iface = self
            .conn()
            .object_server()
            .interface::<_, PairedDevice>(&path)
            .await
            .with_context(|| format!("no paired device {address}"))?;
        anyhow::ensure!(iface.get().await.connected, "{address} is not connected");
        Ok(path)
    }

    /// Puts an A2DP transport under `device`, as the audio server sets it
    /// up: at `sep1/fd<n>`, under the endpoint it was set up through, `n` new
    /// to the device. Announced with `InterfacesAdded`, `active`, at the
    /// device's own volume.
    async fn add_transport(
        &self,
        address: Address,
        device: &OwnedObjectPath,
    ) -> Result<OwnedObjectPath> {
        let (fd, volume) = {
            let mut state = self.state();
            let pairing = state
                .paired
                .get_mut(&address)
                .with_context(|| format!("no paired device {address}"))?;
            pairing.transports += 1;
            (pairing.transports - 1, pairing.volume)
        };
        let path = OwnedObjectPath::try_from(format!("{device}/sep1/fd{fd}"))?;
        let transport = MediaTransport1 {
            bluez: self.clone(),
            path: path.clone(),
            address,
            device: device.clone(),
            active: true,
            volume,
        };
        self.conn().object_server().at(&path, transport).await?;
        Ok(path)
    }

    /// Whether the transport at `path` is `active` or `idle`.
    async fn transport_state(&self, path: &OwnedObjectPath) -> Result<&'static str> {
        let iface = self
            .conn()
            .object_server()
            .interface::<_, MediaTransport1>(path)
            .await?;
        Ok(iface.get().await.state_name())
    }

    /// Makes the transport at `path` `active` or `idle`, signaling a change.
    async fn set_transport_state(&self, path: &OwnedObjectPath, active: bool) -> Result<()> {
        let iface = self
            .conn()
            .object_server()
            .interface::<_, MediaTransport1>(path)
            .await?;
        let mut transport = iface.get_mut().await;
        if transport.active != active {
            transport.active = active;
            transport.state_changed(iface.signal_emitter()).await?;
        }
        Ok(())
    }

    /// Takes a device's transports away, as its disconnect does.
    async fn drop_audio(&self, address: Address) {
        let Some(audio) = self.state().audio.remove(&address) else {
            return;
        };
        let conn = self.conn();
        if let Some(path) = audio.a2dp {
            let _ = conn
                .object_server()
                .remove::<MediaTransport1, _>(&path)
                .await;
        }
    }

    /// Records a volume a client set on a device's transport, and says how
    /// the device answers it.
    fn volume_set(&self, address: Address, volume: u16) -> VolumeAnswer {
        let mut state = self.state();
        state
            .volume_sets
            .entry(address)
            .or_default()
            .push((Instant::now(), volume));
        state
            .paired
            .get(&address)
            .map(|pairing| pairing.answer)
            .unwrap_or_default()
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
    /// Signaled as the module docs describe: returns whether any adapter
    /// heard it.
    pub async fn advertise(&self, address: Address, apple_data: &[u8]) -> Result<bool> {
        let _hearing = self.hearing.lock().await;
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
    /// device it makes of a new address, or, for one it heard before, its
    /// `RSSI` and whatever else changed - see the module docs.
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
                // One signal, as bluetoothd sends it: the strength every time,
                // and the data with it if it changed.
                let mut changed = HashMap::from([("RSSI", Value::from(advertiser.rssi()))]);
                if advertiser.apple_data != apple_data {
                    advertiser.apple_data = apple_data.to_vec();
                    let data = Value::from(advertiser.manufacturer_data());
                    changed.insert("ManufacturerData", data);
                }
                zbus::fdo::Properties::properties_changed(
                    iface.signal_emitter(),
                    InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE),
                    changed,
                    Cow::Borrowed(&[]),
                )
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
        let _hearing = self.hearing.lock().await;
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

    /// What bluetoothd's `discovery_cleanup` does once the last client
    /// discovering on the adapter called `adapter` has stopped, or left the
    /// bus: the devices it only heard advertising go - see
    /// [`FakeBluez::remove_advertisers`]. Nothing goes if a client has
    /// started discovering there since. That is checked under the lock the
    /// devices are forgotten under, and with [`FakeBluez::advertise`] held
    /// off until they are gone, so none goes that a discovery started
    /// meanwhile has heard.
    async fn discovery_ended(&self, adapter: &str) {
        let _hearing = self.hearing.lock().await;
        let heard = {
            let mut state = self.state();
            if state.is_discovering(adapter) {
                return;
            }
            state.forget_advertisers(|heard_by| heard_by == adapter)
        };
        self.remove_advertisers(heard).await;
    }

    /// Removes devices only heard advertising, as bluetoothd's
    /// `discovery_cleanup` does - see the module docs: device by device, a
    /// `PropertiesChanged` naming `RSSI` among the invalidated properties,
    /// then its `InterfacesRemoved`. bluetoothd queues every invalidation
    /// first, and removing a device sends that device's queued changes and
    /// then its removal at once (`remove_interface` and `process_changes` in
    /// `gdbus/object.c`), so on the wire each device's two signals come
    /// together. Called with `hearing` held.
    async fn remove_advertisers(&self, heard: Vec<OwnedObjectPath>) {
        let conn = self.conn();
        let server = conn.object_server();
        for path in heard {
            if let Ok(iface) = server.interface::<_, Advertiser>(&path).await {
                let _ = zbus::fdo::Properties::properties_changed(
                    iface.signal_emitter(),
                    InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE),
                    HashMap::new(),
                    Cow::Borrowed(&["RSSI"]),
                )
                .await;
            }
            let _ = server.remove::<Advertiser, _>(&path).await;
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

    /// A connection left the bus: its discovery stops on every adapter - the
    /// last client's ending discovery there, as
    /// [`FakeBluez::discovery_ended`] has it - and its providers go.
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
            self.discovery_ended(&adapter).await;
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
        // The devices go before `Discovering` is signaled, as in bluetoothd.
        // With discovery active, bluetoothd sends this reply before the
        // cleanup's signals, and the fake after them: nothing the app sees
        // changes.
        if stopped {
            self.bluez.discovery_ended(&self.name).await;
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
    services_resolved: bool,
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

    #[zbus(property)]
    fn services_resolved(&self) -> bool {
        self.services_resolved
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

/// The A2DP transport the audio server set up with a device - see
/// [Audio](#audio).
struct MediaTransport1 {
    bluez: FakeBluez,
    /// Its own path, for an answer that comes late.
    path: OwnedObjectPath,
    address: Address,
    device: OwnedObjectPath,
    /// `active` while it streams, `idle` once released.
    active: bool,
    volume: u16,
}

impl MediaTransport1 {
    fn state_name(&self) -> &'static str {
        if self.active { "active" } else { "idle" }
    }
}

#[interface(name = "org.bluez.MediaTransport1")]
impl MediaTransport1 {
    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        self.device.clone()
    }

    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        A2DP_SOURCE_UUID.to_string()
    }

    /// AAC, as the pods have it with PipeWire; nothing reads it.
    #[zbus(property)]
    fn codec(&self) -> u8 {
        0x02
    }

    #[zbus(property)]
    fn state(&self) -> String {
        self.state_name().to_string()
    }

    #[zbus(property)]
    fn volume(&self) -> u16 {
        self.volume
    }

    /// Writable by any client, as bluetoothd has it whatever its
    /// documentation says. Recorded as a client's - see
    /// [`FakeBluez::volume_sets`] - and answered as the device answers - see
    /// [`FakeBluez::set_volume_answer`]: at once, and then signaled, as zbus
    /// signals every property set; or later, signaled then. The volume held
    /// already changes nothing, as in bluetoothd.
    #[zbus(property)]
    fn set_volume(&mut self, volume: u16) -> zbus::fdo::Result<()> {
        if volume > MAX_VOLUME {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "Invalid volume value {volume}"
            )));
        }
        let answer = self.bluez.volume_set(self.address, volume);
        if volume == self.volume {
            return Ok(());
        }
        let answered = answer.answered(volume);
        if answer.delay.is_zero() {
            self.volume = answered;
            if let Some(pairing) = self.bluez.state().paired.get_mut(&self.address) {
                pairing.volume = answered;
            }
            return Ok(());
        }
        let (bluez, path, address) = (self.bluez.clone(), self.path.clone(), self.address);
        tokio::spawn(async move {
            tokio::time::sleep(answer.delay).await;
            if let Err(e) = bluez.answer_volume(&path, address, answered).await {
                tracing::warn!("fake BlueZ: failed to answer the volume of {address}: {e:#}");
            }
        });
        Ok(())
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
    use crate::ble::parser::PodSide;
    use crate::ble::scanner::Scanner;
    use crate::bluez::{self, BatteryAction, BatteryProvider, Devices};
    use crate::harness::PrivateBus;
    use crate::harness::pods::{PRO_3, Part, Pods};

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
        let bluez = FakeBluez::serve(bus.address()).await.unwrap();
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

    /// Waits until `done`, which the fake gets to by itself, asking it over
    /// an await.
    async fn eventually_async<F, Fut>(done: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        timeout(TIMEOUT, async {
            while !done().await {
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
        let info = devices.device_info(&path).await.unwrap();
        assert_eq!(info.address, PAIR);
        assert_eq!(info.adapter.unwrap(), adapter_path(ADAPTER));
        assert!(info.connected.unwrap());
        assert_eq!(devices.device_alias(&path).await, ALIAS);
        assert!(devices.device_is_airpods(&path).await);
        assert!(devices.device_connected(&path).await.unwrap());

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
        let bluez = FakeBluez::serve(bus.address()).await.unwrap();
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
        let ad = pods.advertisement(PodSide::Right, Duration::ZERO);
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
        bluez.restart(bus.address()).await.unwrap();
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
        let bluez = FakeBluez::serve(bus.address()).await.unwrap();
        bluez.publish().await.unwrap();
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let err = scanner.start_discovery().await.unwrap_err();
        assert_eq!(err.to_string(), "BlueZ has no Bluetooth adapter");

        bluez.add_adapter(ADAPTER, false).await.unwrap();
        let err = scanner.start_discovery().await.unwrap_err();
        assert_eq!(err.to_string(), "no Bluetooth adapter is scanning");
    }

    /// A connection changing is signaled; setting it as it was is not.
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

    /// A connection has its services resolved a while after `Connected=true`,
    /// and they go just before `Connected=false`, each in a signal of its own,
    /// as in the #71 captures. A connection that goes before its services are
    /// resolved never has them: the next one waits its own delay.
    /// The changes the pair's device signals, each one property and its new
    /// value, from a connection to the bus at `bus`.
    async fn device_changes(bus: &PrivateBus) -> impl futures_util::Stream<Item = (String, bool)> {
        let client = bus.connect().await.unwrap();
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .unwrap()
            .member("PropertiesChanged")
            .unwrap()
            .path(device_path(ADAPTER, PAIR))
            .unwrap()
            .build();
        let signals = MessageStream::for_match_rule(rule, &client, None)
            .await
            .unwrap();
        signals.map(|msg| {
            let (_, props, _) = msg
                .unwrap()
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .unwrap();
            assert_eq!(props.len(), 1, "{props:?}");
            let (name, value) = props.into_iter().next().unwrap();
            (name, bool::try_from(value).unwrap())
        })
    }

    fn changed(name: &str, value: bool) -> (String, bool) {
        (name.to_string(), value)
    }

    /// A connection has its services resolved a while after `Connected=true`,
    /// and they go just before `Connected=false`, each in a signal of its own,
    /// as in the #71 captures. A connection that goes before its services are
    /// resolved never has them: the next one waits its own delay, longer
    /// than the first's, so that a timer resolving the wrong connection would
    /// show by coming first.
    #[tokio::test]
    async fn services_are_resolved_a_while_after_a_connect() {
        const DELAY: Duration = Duration::from_millis(100);
        // Long enough that the quick connection is gone before it is over.
        const QUICK: Duration = Duration::from_secs(1);
        const LAST: Duration = Duration::from_millis(1500);
        let (bus, bluez) = served().await;
        bluez.set_services_delay(DELAY);
        let changes = device_changes(&bus).await;
        tokio::pin!(changes);
        let mut next = async || timeout(TIMEOUT, changes.next()).await.unwrap().unwrap();

        let connected = Instant::now();
        bluez.set_connected(PAIR, true).await.unwrap();
        assert_eq!(next().await, changed("Connected", true));
        assert_eq!(next().await, changed("ServicesResolved", true));
        assert!(connected.elapsed() >= DELAY);
        bluez.set_connected(PAIR, false).await.unwrap();
        assert_eq!(next().await, changed("ServicesResolved", false));
        assert_eq!(next().await, changed("Connected", false));

        bluez.set_services_delay(QUICK);
        bluez.set_connected(PAIR, true).await.unwrap();
        bluez.set_connected(PAIR, false).await.unwrap();
        bluez.set_services_delay(LAST);
        let connected = Instant::now();
        bluez.set_connected(PAIR, true).await.unwrap();
        for value in [true, false, true] {
            assert_eq!(next().await, changed("Connected", value));
        }
        assert_eq!(next().await, changed("ServicesResolved", true));
        assert!(connected.elapsed() >= LAST, "the quick connection's");
    }

    /// A connection's timer outlives its device's object going with an
    /// unplugged adapter and coming back with it, and resolves nothing for
    /// the next connection: that one waits its own, longer delay.
    #[tokio::test]
    async fn services_resolve_for_their_own_connection_across_an_unplug() {
        const FIRST: Duration = Duration::from_secs(1);
        const NEXT: Duration = Duration::from_millis(1500);
        let (bus, bluez) = served().await;
        let changes = device_changes(&bus).await;
        tokio::pin!(changes);
        let mut next = async || timeout(TIMEOUT, changes.next()).await.unwrap().unwrap();

        bluez.set_services_delay(FIRST);
        bluez.set_connected(PAIR, true).await.unwrap();
        bluez.remove_adapter(ADAPTER).await.unwrap();
        bluez.add_adapter(ADAPTER, true).await.unwrap();
        bluez.set_services_delay(NEXT);
        let connected = Instant::now();
        bluez.set_connected(PAIR, true).await.unwrap();
        for value in [true, false, true] {
            assert_eq!(next().await, changed("Connected", value));
        }
        assert_eq!(next().await, changed("ServicesResolved", true));
        assert!(connected.elapsed() >= NEXT, "the first connection's");
    }

    /// Without the AirPods service a pair is not AirPods to the app, connected
    /// or not; listing it is signaled, and reaches the watch as a change of
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

    /// A new address appears with its data; each later advertisement brings
    /// its strength, and its data with it only when that changed; and an
    /// address gone quiet goes - and nothing is heard while no one discovers.
    #[tokio::test]
    async fn advertisements_are_signaled_as_bluetoothd_does() {
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
        assert!(bluez.advertise(RANDOM, &[0x04, 0x05]).await.unwrap());
        bluez.prune(Instant::now()).await;
        bluez.prune(Instant::now() + TEMPORARY_TIMEOUT).await;

        let path = device_path(ADAPTER, RANDOM);
        let adapter = adapter_path(ADAPTER);
        let mut device_signals = Vec::new();
        while device_signals.len() < 4 {
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
        assert_eq!(i16::try_from(device["RSSI"].clone()).unwrap(), RSSI);
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

        let changed = |msg: &zbus::message::Message| {
            assert_eq!(msg.header().member().unwrap().as_str(), "PropertiesChanged");
            assert_eq!(msg.header().path().unwrap().as_str(), path.as_str());
            let (iface, props, _) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .unwrap();
            assert_eq!(iface, DEVICE_INTERFACE);
            assert_eq!(i16::try_from(props["RSSI"].clone()).unwrap(), RSSI);
            props
        };
        let new_data = changed(&device_signals[1]);
        assert_eq!(apple_data(&new_data), [0x04, 0x05]);
        let repeated = changed(&device_signals[2]);
        assert_eq!(
            repeated.keys().collect::<Vec<_>>(),
            ["RSSI"],
            "the same data again"
        );

        let removed = &device_signals[3];
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
                let info = devices.device_info(path.as_str()).await.ok()?;
                Some((info.address, info.adapter.ok()?))
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

    /// The scanner takes what the fake signals, every payload that changed
    /// and none that did not: the plumbing the pipeline tests (#73) build on.
    #[tokio::test]
    async fn the_scanner_reads_its_advertisements() {
        let (bus, bluez) = served().await;
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        scanner.start_discovery().await.unwrap();

        // From the one address: as the object appears, the same again - the
        // strength alone - and a change. The scanner reads the first and the
        // change, in order, or the change alone when its reader is behind.
        let mut pods = Pods::new(PRO_3, PAIR);
        let first = pods.advertisement(PodSide::Right, Duration::ZERO);
        let address = first.address;
        pods.set_battery(Part::Left, Some(10));
        let changed = pods.advertisement(PodSide::Right, Duration::ZERO);
        for ad in [&first, &first, &changed] {
            bluez.advertise(address, &ad.data).await.unwrap();
        }
        let mut next = async || timeout(TIMEOUT, adverts.next()).await.unwrap().unwrap();
        let mut advert = next().await;
        assert_eq!(advert.ble_mac, address);
        assert_eq!(advert.data.product_id, PRO_3);
        if advert.data.left_battery == Some(80) {
            advert = next().await;
        }
        assert_eq!(advert.data.left_battery, Some(10), "the change");
    }

    /// The devices of a pair's pods go when the last client stops
    /// discovering, as bluetoothd removes them in `discovery_cleanup` (#153):
    /// device by device, its `RSSI` invalidated and then its
    /// `InterfacesRemoved`, and `Discovering` after all of them. Not while
    /// another client discovers, and never a paired device, connected or not.
    /// A client leaving the bus ends its discovery as stopping does.
    #[tokio::test]
    async fn the_last_client_to_stop_discovering_takes_the_pods_devices() {
        let (bus, bluez) = served().await;
        let (a, b) = (bus.connect().await.unwrap(), bus.connect().await.unwrap());
        let watcher = bus.connect().await.unwrap();
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)
            .unwrap()
            .build();
        // Subscribed once this is back, before anything below is signaled.
        let mut signals = MessageStream::for_match_rule(rule, &watcher, None)
            .await
            .unwrap();
        let devices = Devices::new(watcher.clone());
        let path = adapter_path(ADAPTER);
        let adapter_a = proxy(&a, &path, "org.bluez.Adapter1").await;
        let adapter_b = proxy(&b, &path, "org.bluez.Adapter1").await;
        for adapter in [&adapter_a, &adapter_b] {
            adapter.call_method("StartDiscovery", &()).await.unwrap();
        }
        bluez.set_connected(PAIR, true).await.unwrap();
        let mut pods = Pods::new(PRO_3, OTHER);
        let heard: HashSet<Address> = pods
            .advertisements(Duration::ZERO)
            .iter()
            .map(|ad| ad.address)
            .collect();
        assert_eq!(heard.len(), 2, "both pods in the open case");
        let advertise_all = async |pods: &mut Pods| {
            for ad in pods.advertisements(Duration::ZERO) {
                assert!(bluez.advertise(ad.address, &ad.data).await.unwrap());
            }
        };
        advertise_all(&mut pods).await;
        let pair = device_path(ADAPTER, PAIR);

        adapter_b.call_method("StopDiscovery", &()).await.unwrap();
        assert_eq!(bluez.advertisers_on(ADAPTER), heard, "kept for a");

        adapter_a.call_method("StopDiscovery", &()).await.unwrap();
        assert!(bluez.advertisers_on(ADAPTER).is_empty());
        for address in &heard {
            let path = device_path(ADAPTER, *address);
            assert!(devices.device_info(path.as_str()).await.is_err(), "gone");
        }
        assert!(devices.device_connected(pair.as_str()).await.unwrap());

        // What `a`'s stop signaled, after what the advertisements did.
        let device_paths: HashSet<OwnedObjectPath> = heard
            .iter()
            .map(|address| device_path(ADAPTER, *address))
            .collect();
        let mut invalidated = HashSet::new();
        let mut removed: HashSet<OwnedObjectPath> = HashSet::new();
        loop {
            let msg = timeout(TIMEOUT, signals.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let header = msg.header();
            let (Some(member), Some(at)) = (header.member(), header.path()) else {
                continue;
            };
            match member.as_str() {
                "PropertiesChanged" if at.as_str() == path => {
                    let (_, changed, _) = msg
                        .body()
                        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        .unwrap();
                    let Some(discovering) = changed.get("Discovering") else {
                        continue;
                    };
                    if !bool::try_from(discovering.clone()).unwrap() {
                        break;
                    }
                }
                "PropertiesChanged" if device_paths.iter().any(|p| p.as_str() == at.as_str()) => {
                    let (_, changed, gone) = msg
                        .body()
                        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        .unwrap();
                    if gone.is_empty() {
                        continue;
                    }
                    assert!(changed.is_empty());
                    assert_eq!(gone, ["RSSI"]);
                    let at = OwnedObjectPath::from(at.to_owned());
                    assert!(!removed.contains(&at), "invalidated before it goes");
                    invalidated.insert(at);
                }
                "InterfacesRemoved" => {
                    let (object, _) = msg
                        .body()
                        .deserialize::<(OwnedObjectPath, Vec<String>)>()
                        .unwrap();
                    assert_ne!(object, pair, "a paired device stays");
                    assert!(invalidated.contains(&object), "invalidated first");
                    removed.insert(object);
                }
                _ => {}
            }
        }
        assert_eq!(invalidated, device_paths);
        assert_eq!(removed, device_paths, "removed before Discovering");

        // Heard afresh by the next discovery, and gone as its client leaves.
        adapter_b.call_method("StartDiscovery", &()).await.unwrap();
        advertise_all(&mut pods).await;
        assert_eq!(bluez.advertisers_on(ADAPTER), heard);
        drop((adapter_b, b));
        eventually(|| bluez.advertisers_on(ADAPTER).is_empty()).await;
        assert!(!bluez.is_discovering(ADAPTER));
    }

    /// The end of a discovery takes nothing once a client has started
    /// discovering again, which the removal checks for under the lock it
    /// forgets the devices under.
    #[tokio::test]
    async fn the_end_of_a_discovery_takes_nothing_while_a_client_discovers() {
        let (bus, bluez) = served().await;
        let client = bus.connect().await.unwrap();
        Scanner::with_connection(client.clone())
            .start_discovery()
            .await
            .unwrap();
        assert!(bluez.advertise(RANDOM, &[0x07]).await.unwrap());
        bluez.discovery_ended(ADAPTER).await;
        assert_eq!(bluez.advertisers_on(ADAPTER), HashSet::from([RANDOM]));
        let devices = Devices::new(client);
        let path = device_path(ADAPTER, RANDOM);
        assert!(devices.device_info(path.as_str()).await.is_ok());
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
        let mut provider = BatteryProvider::register(
            &bus.connect_for_providers().await.unwrap(),
            &adapter_path(ADAPTER),
        )
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

    /// BlueZ's read of a provider's root is answered however late the
    /// runtime gets round to zbus's object server, on a connection from
    /// `bluez::connection`: its object server dispatches from the start.
    /// Regression (#125): on a connection built plain, the provider's own
    /// `object_server()` started the dispatch, in a task that under load
    /// subscribed only after the fake's `GetManagedObjects` had come. zbus
    /// dropped the call unanswered, and the fake, waiting for the reply, took
    /// no battery from the provider. Here the runtime such a task would be
    /// spawned on runs nothing until the call is in.
    #[tokio::test]
    async fn a_providers_root_is_read_however_late_its_object_server_would_start() {
        let (bus, bluez) = served().await;
        let conn = bus.connect_for_providers().await.unwrap();
        // The read as the connection takes it in, whoever else does.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::MethodCall)
            .destination(conn.unique_name().unwrap().clone())
            .unwrap()
            .member("GetManagedObjects")
            .unwrap()
            .build();
        let mut reads = MessageStream::for_match_rule(rule, &conn, None)
            .await
            .unwrap();
        // The first `object_server()` spawns the dispatch task, if anything
        // does: on this runtime, whatever calls it.
        let held = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        {
            let _held = held.enter();
            conn.object_server();
        }

        let mut provider = BatteryProvider::register(&conn, &adapter_path(ADAPTER))
            .await
            .unwrap();
        timeout(TIMEOUT, reads.next())
            .await
            .expect("the fake reads the root")
            .unwrap()
            .unwrap();
        // Late, but running from here on.
        let (_release, released) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            held.block_on(async {
                let _ = released.await;
            });
        });
        let add = BatteryAction::Add {
            mac: PAIR,
            device_path: device_path(ADAPTER, PAIR).to_string(),
            percentage: 73,
        };
        provider.apply(&add).await.unwrap();
        eventually(|| bluez.batteries() == HashMap::from([(PAIR, 73)])).await;
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
        let mut provider = BatteryProvider::register(
            &bus.connect_for_providers().await.unwrap(),
            &adapter_path(ADAPTER),
        )
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

        let conn = bus.connect_for_providers().await.unwrap();
        let mut first = BatteryProvider::register(&conn, &adapter_path(ADAPTER))
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

        let mut second = BatteryProvider::register(&conn, &adapter_path(SECOND))
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
        let mut provider = BatteryProvider::register(
            &bus.connect_for_providers().await.unwrap(),
            &adapter_path(ADAPTER),
        )
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
    /// providers stay. Each change is signaled.
    #[tokio::test]
    async fn powering_off_stops_the_adapter() {
        let (bus, bluez) = served().await;
        let conn = bus.connect_for_providers().await.unwrap();
        let devices = Devices::new(Connection::clone(&conn));
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
        let _provider = BatteryProvider::register(&conn, &path).await.unwrap();
        Scanner::with_connection(Connection::clone(&conn))
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
            devices
                .device_connected(device_path(ADAPTER, RANDOM).as_str())
                .await
                .is_err(),
            "gone"
        );
        let pair = device_path(ADAPTER, PAIR).to_string();
        assert!(!devices.device_connected(&pair).await.unwrap());
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
        assert!(
            !devices.device_connected(&pair).await.unwrap(),
            "not by itself"
        );
        bluez.set_connected(PAIR, true).await.unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            seen.push(timeout(TIMEOUT, powered.next()).await.unwrap().unwrap());
        }
        assert_eq!(seen, [false, true], "each change once");
        assert!(bluez.set_powered("hci9", true).await.is_err(), "no adapter");
    }

    /// An adapter is plugged in and out at any time, each signaled. Out, it
    /// takes its devices, providers and batteries along and leaves the other
    /// adapter alone; in again, it is the same controller, with its pairs back,
    /// disconnected.
    #[tokio::test]
    async fn adapters_are_plugged_in_and_out() {
        let (bus, bluez) = served().await;
        let conn = bus.connect_for_providers().await.unwrap();
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
        let mut provider = BatteryProvider::register(&conn, &second).await.unwrap();
        let first = BatteryProvider::register(&conn, &adapter_path(ADAPTER))
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
        let devices = Devices::new(Connection::clone(&conn));
        assert!(
            devices
                .device_info(device_path(SECOND, OTHER).as_str())
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
        assert_eq!(devices.device_info(&other).await.unwrap().address, OTHER);
        assert!(!devices.device_connected(&other).await.unwrap());
        assert_eq!(devices.device_alias(&other).await, ALIAS);
    }

    /// Restarted, the fake BlueZ lets go of its name and takes it again, and
    /// forgets its clients' discovery and providers, and what it only heard
    /// advertising. The adapters and connections are as they were.
    #[tokio::test]
    async fn a_restart_forgets_every_client() {
        let (bus, bluez) = served().await;
        let conn = bus.connect_for_providers().await.unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        let mut owners = dbus
            .receive_name_owner_changed_with_args(&[(0, BLUEZ_SERVICE)])
            .await
            .unwrap();
        let mut provider = BatteryProvider::register(&conn, &adapter_path(ADAPTER))
            .await
            .unwrap();
        Scanner::with_connection(Connection::clone(&conn))
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
        bluez.restart(bus.address()).await.unwrap();
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
        let devices = Devices::new(Connection::clone(&conn));
        assert!(
            devices
                .device_connected(device_path(ADAPTER, PAIR).as_str())
                .await
                .unwrap()
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
        BatteryProvider::register(&conn, &adapter_path(ADAPTER))
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
        let bluez = FakeBluez::serve(bus.address()).await.unwrap();
        bluez.add_adapter(ADAPTER, true).await.unwrap();
        bluez.publish().await.unwrap();
        assert_eq!(bus.activations(), 0, "owned already");

        bluez.stop().await.unwrap();
        let client = bus.connect().await.unwrap();
        let asked = tokio::spawn(async move { bluez::adapters(&client).await });
        eventually(|| bus.activations() == 1).await;
        assert!(!asked.is_finished(), "answered while away");
        bluez.start(bus.address()).await.unwrap();
        let adapters = timeout(TIMEOUT, asked).await.unwrap().unwrap().unwrap();
        assert_eq!(adapters.len(), 1);
    }

    /// Asked whether a device is connected while BlueZ is away, on a bus
    /// that activates it, `Devices` gets no answer and starts nothing: a
    /// connection event, forged or stale, must not start bluetoothd (#132).
    #[tokio::test]
    async fn asking_whether_a_device_is_connected_never_activates_bluez() {
        let bus = PrivateBus::activating(BLUEZ_SERVICE).unwrap();
        let bluez = FakeBluez::serve(bus.address()).await.unwrap();
        bluez.add_adapter(ADAPTER, true).await.unwrap();
        bluez.add_device(ADAPTER, PAIR, ALIAS).await.unwrap();
        bluez.publish().await.unwrap();
        let devices = Devices::new(bus.connect().await.unwrap());
        let path = device_path(ADAPTER, PAIR).to_string();
        assert!(!devices.device_connected(&path).await.unwrap());

        bluez.stop().await.unwrap();
        assert!(devices.device_connected(&path).await.is_err());
        assert_eq!(bus.activations(), 0);
    }

    /// Without the battery API there is no manager: registering fails as the
    /// method is unknown, and the adapter is there all the same.
    #[tokio::test]
    async fn an_adapter_can_lack_the_battery_api() {
        let (bus, bluez) = served().await;
        bluez.drop_battery_api(ADAPTER).await.unwrap();
        let conn = bus.connect_for_providers().await.unwrap();
        let refused = BatteryProvider::register(&conn, &adapter_path(ADAPTER)).await;
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

    /// What `volume` reads and sets, the fake serves as bluetoothd does: the
    /// pair's A2DP transport under its device while it plays, at a path new
    /// with each setup, its volume writable by any client and signaled, idle
    /// in a call, and none once it disconnects.
    #[tokio::test]
    async fn audio_transports_come_and_go_with_the_audio() {
        use crate::volume::{find_a2dp, set_volume};

        let (bus, bluez) = served().await;
        let conn = bus.connect().await.unwrap();
        assert!(
            bluez.set_playing(PAIR, true).await.is_err(),
            "not connected"
        );
        bluez.set_connected(PAIR, true).await.unwrap();
        assert_eq!(find_a2dp(&conn, PAIR).await.unwrap(), None);

        bluez.set_playing(PAIR, true).await.unwrap();
        let a2dp = find_a2dp(&conn, PAIR).await.unwrap().unwrap();
        let device = device_path(ADAPTER, PAIR);
        assert_eq!(a2dp.path.as_str(), format!("{device}/sep1/fd0"));
        assert_eq!(
            (a2dp.transport.volume, a2dp.transport.playing),
            (DEFAULT_VOLUME, true)
        );

        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .member("PropertiesChanged")
            .unwrap()
            .path(a2dp.path.as_str())
            .unwrap()
            .build();
        let mut changes = MessageStream::for_match_rule(rule, &conn, None)
            .await
            .unwrap();
        set_volume(&conn, &a2dp.path, 13).await.unwrap();
        let changed = timeout(TIMEOUT, changes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let (iface, props, _) = changed
            .body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            .unwrap();
        assert_eq!(iface, "org.bluez.MediaTransport1");
        assert_eq!(u16::try_from(props["Volume"].clone()).unwrap(), 13);
        assert!(set_volume(&conn, &a2dp.path, 128).await.is_err());
        bluez.set_volume(PAIR, 40).await.unwrap();
        let sets: Vec<u16> = bluez
            .volume_sets(PAIR)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        assert_eq!(sets, [13], "the user's is not a client's");
        assert_eq!(bluez.volume(PAIR).await, Some(40));

        // A call: A2DP idle, and back after it.
        bluez.set_call(PAIR, true).await.unwrap();
        assert!(
            !find_a2dp(&conn, PAIR)
                .await
                .unwrap()
                .unwrap()
                .transport
                .playing
        );
        bluez.set_call(PAIR, false).await.unwrap();
        assert!(
            find_a2dp(&conn, PAIR)
                .await
                .unwrap()
                .unwrap()
                .transport
                .playing
        );
        bluez.set_playing(PAIR, false).await.unwrap();
        assert!(
            !find_a2dp(&conn, PAIR)
                .await
                .unwrap()
                .unwrap()
                .transport
                .playing
        );

        // Gone with the connection; the next setup is at a new path, at the
        // volume the pair kept.
        bluez.set_connected(PAIR, false).await.unwrap();
        assert_eq!(find_a2dp(&conn, PAIR).await.unwrap(), None);
        bluez.set_connected(PAIR, true).await.unwrap();
        bluez.set_playing(PAIR, true).await.unwrap();
        let next = find_a2dp(&conn, PAIR).await.unwrap().unwrap();
        assert_ne!(next.path, a2dp.path);
        assert_eq!(next.transport.volume, 40);

        // Answered late and rounded, as bluetoothd has real pods answer: the
        // transport reads the old volume until the answer, then the even one.
        // The volume it holds already changes nothing.
        let answer = VolumeAnswer {
            delay: Duration::from_millis(100),
            to_even: true,
        };
        bluez.set_volume_answer(PAIR, answer).unwrap();
        set_volume(&conn, &next.path, 25).await.unwrap();
        assert_eq!(bluez.volume(PAIR).await, Some(40), "not answered yet");
        eventually_async(|| async { bluez.volume(PAIR).await == Some(24) }).await;
        set_volume(&conn, &next.path, 24).await.unwrap();
        let sets: Vec<u16> = bluez
            .volume_sets(PAIR)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        assert_eq!(sets, [13, 25, 24]);
        tokio::time::sleep(answer.delay * 2).await;
        assert_eq!(bluez.volume(PAIR).await, Some(24), "nothing to answer");
    }
}
