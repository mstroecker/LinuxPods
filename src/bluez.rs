//! BlueZ integration: its adapters, read by [`adapters`] and followed by
//! [`watch_adapters`], its device objects, read and watched by [`Devices`],
//! and the batteries [`BatteryProvider`] exports to it for GNOME Settings.
//! All go over a connection they are given - the system bus, or under the
//! test harness a fake BlueZ on a private one (#72).
//!
//! zbus ships `fdo::ObjectManager` and derives properties, so serving the
//! provider root needs none of the usual ObjectManager boilerplate - no
//! GetManagedObjects of our own, no introspection XML, no manual
//! InterfacesAdded / InterfacesRemoved / PropertiesChanged: registering the
//! interface at a child path emits InterfacesAdded on its own.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, ready};
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, watch};
use zbus::fdo::ManagedObjects;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream, interface, proxy};

pub const BLUEZ_SERVICE: &str = "org.bluez";
const ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";
const BATTERY_NAME: &str = "airpods_battery";
const SOURCE: &str = "LinuxPods";

/// The service UUID every AirPods lists in `Device1.UUIDs` - the identifier
/// LibrePods uses too. Unlike the alias it survives a rename, and unlike Apple's
/// vendor ID in `Modalias` it does not also match Apple keyboards and mice.
pub const AIRPODS_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

/// On every adapter's object, each keeping the providers registered with it.
#[proxy(
    interface = "org.bluez.BatteryProviderManager1",
    default_service = "org.bluez"
)]
trait BatteryProviderManager1 {
    fn register_battery_provider(&self, provider: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device1 {
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn adapter(&self) -> zbus::Result<OwnedObjectPath>;
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

/// One adapter's registered provider, exporting one battery object per device
/// on that adapter.
///
/// Each adapter's `BatteryProviderManager1` keeps providers of its own, and
/// takes a battery only for a device on its own adapter: bluetoothd looks the
/// battery's `Device` up among that adapter's devices, and ignores the battery,
/// with a warning in its log, when it is not one of them (`src/battery.c`). So
/// every adapter has a provider of its own, at a root of its own - and of the
/// connection's own, see [`provider_root`]. One root
/// registered with every adapter would work too, but every manager would read
/// every battery, and warn of each on another adapter.
///
/// Which objects are exported, and at what level, is the caller's to keep:
/// `attachment` keeps it with the rest of what it knows about each pair, so
/// the two cannot drift apart.
pub struct BatteryProvider {
    conn: Connection,
    /// Where its batteries are - see [`provider_root`].
    root: OwnedObjectPath,
}

/// A connection [`BatteryProvider`]s can be served on, built by
/// [`connection`] and only there: a provider takes nothing else, so none is
/// served on a connection built plain. Dereferences to the connection.
#[derive(Clone, Debug)]
pub struct ProviderConnection(Connection);

impl std::ops::Deref for ProviderConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.0
    }
}

/// The connection [`BatteryProvider`]s are served on, from `builder`: the
/// system bus's, or under the test harness a private bus's.
///
/// bluetoothd reads a provider's root with `GetManagedObjects` as it takes
/// the registration, and a call the object server misses is never answered.
/// On a connection built plain, zbus starts dispatching method calls in a
/// task that the first `object_server()` spawns, and drops a method call
/// that comes before that task has subscribed - no reply, no error, nothing
/// in any log. Signals still reach their own subscriptions. The provider's
/// own registration made that first call, and under load the task
/// subscribed only after the read had come (#125). bluetoothd waits 25 s for
/// its reply and takes the provider's `InterfacesAdded` meanwhile, so it
/// would miss only a battery the root held already - and there is none, as
/// attachment registers a provider before it exports to it. The fake BlueZ,
/// which takes no signal before the reply, took no battery at all.
///
/// Built with something to serve, a connection has the task subscribed
/// before it reads its first message. The ObjectManager over every provider
/// root served here is there for that alone: it never emits, as zbus emits
/// from the ObjectManager closest to an object, each root's own.
pub async fn connection(builder: zbus::connection::Builder<'_>) -> Result<ProviderConnection> {
    let conn = builder
        .serve_at(provider_path(), zbus::fdo::ObjectManager)?
        .build()
        .await?;
    Ok(ProviderConnection(conn))
}

/// Where every provider root goes, in the app's namespace.
fn provider_path() -> String {
    format!("{}/battery", crate::APP_PATH)
}

/// Where the provider for the adapter at `adapter`, served by the connection
/// whose unique name is `owner`, keeps its batteries: a root named after both,
/// as `.../battery/_1_42/hci1` for `/org/bluez/hci1` and `:1.42`.
/// bluetoothd reads a provider's objects from the ObjectManager at its root.
///
/// bluetoothd tells providers apart by their root alone, whoever registered
/// it. A root of one connection's own is one no other can hold: two LinuxPods
/// on one system bus - two users logged in - each get a provider, and BlueZ
/// refusing a root as registered already can only mean this connection's.
fn provider_root(owner: &str, adapter: &str) -> Result<OwnedObjectPath> {
    let name = adapter.rsplit('/').next().unwrap_or_default();
    let owner: String = owner
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = format!("{}/{owner}/{name}", provider_path());
    OwnedObjectPath::try_from(path.as_str())
        .with_context(|| format!("invalid provider root {path} for adapter {adapter}"))
}

/// Where one device's battery object lives, under its provider's root.
fn battery_path(root: &str, mac: Address) -> Result<OwnedObjectPath> {
    let path = format!(
        "{root}/{BATTERY_NAME}_{}",
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
    /// Serves the provider root for the adapter at `adapter` on `conn`, and
    /// registers it with that adapter's manager, in whichever BlueZ answers
    /// there. `conn` is from [`connection`], so that BlueZ's read of the root
    /// is answered.
    ///
    /// The root is this connection's own - see [`provider_root`] - so BlueZ
    /// refusing it as registered already means this connection registered it
    /// before, and it is registered: registering again, after an adapter
    /// change that turned out to change nothing for BlueZ, must not lose the
    /// registration that stands (#50).
    pub async fn register(conn: &ProviderConnection, adapter: &str) -> Result<Self> {
        let conn = Connection::clone(conn);
        let owner = conn
            .unique_name()
            .context("the connection has no unique name")?
            .to_string();
        let root = provider_root(&owner, adapter)?;
        // Serving ObjectManager at the provider root is what makes BlueZ pick up
        // batteries added underneath it.
        conn.object_server()
            .at(&root, zbus::fdo::ObjectManager)
            .await
            .context("failed to serve ObjectManager")?;

        let manager = BatteryProviderManager1Proxy::new(&conn, adapter)
            .await
            .context("failed to reach BatteryProviderManager1")?;
        match manager.register_battery_provider(&root).await {
            Ok(()) => {}
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.bluez.Error.AlreadyExists" =>
            {
                tracing::debug!("{adapter} holds the battery provider at {root} already");
            }
            Err(e) => return Err(e).context("failed to register battery provider"),
        }

        Ok(Self { conn, root })
    }

    /// Carries out one change to the exported battery objects.
    pub(crate) async fn apply(&mut self, action: &BatteryAction) -> Result<()> {
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
        let battery_path = battery_path(&self.root, mac)?;
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
            .interface::<_, BatteryProvider1>(&battery_path(&self.root, mac)?)
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
            .remove::<BatteryProvider1, _>(&battery_path(&self.root, mac)?)
            .await
            .context("failed to remove battery object")?;
        Ok(())
    }
}

/// One of BlueZ's adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// Its object, `/org/bluez/hci0` and on, where `Adapter1` and its
    /// `BatteryProviderManager1` are.
    pub path: String,
    pub powered: bool,
}

/// Every adapter of whichever BlueZ answers on `conn`, powered or not, by
/// path. Which is first, and what it is called, depends on the machine: a USB
/// dongle, a disabled internal adapter or a re-enumeration after resume can
/// leave none at `hci0` (#51).
pub async fn adapters(conn: &Connection) -> Result<Vec<Adapter>> {
    Ok(adapters_in(managed_objects(conn).await?))
}

/// Picks the adapters out of a GetManagedObjects reply: every object with
/// `Adapter1`, by path. A `Powered` that is absent or mistyped reads as off.
fn adapters_in(objects: ManagedObjects) -> Vec<Adapter> {
    let mut adapters: Vec<Adapter> = objects
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get(ADAPTER_INTERFACE)?;
            Some(Adapter {
                path: path.to_string(),
                powered: bool_prop(props, "Powered"),
            })
        })
        .collect();
    adapters.sort_by(|a, b| a.path.cmp(&b.path));
    adapters
}

/// Every object of whichever BlueZ answers on `conn`, with its interfaces and
/// their properties.
async fn managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let om = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ_SERVICE)?
        .path("/")?
        .build()
        .await?;
    om.get_managed_objects()
        .await
        .context("failed to get managed objects")
}

/// BlueZ's adapters as a client needs to know them, to keep what it has on
/// each in step with BlueZ (#50).
///
/// What a client starts on an adapter lasts only as long as BlueZ keeps it.
/// bluetoothd ends every client's discovery on an adapter as it powers off;
/// an adapter that goes takes the battery providers registered with it along;
/// and a new bluetoothd - a restart - knows of neither, for any adapter,
/// saying so only by the name changing hands. So the set holds, for each
/// adapter there is, two tokens: what a client started there under a token
/// stands while the adapter has that token still. A new one is a sign to
/// start again.
///
/// From [`watch_adapters`], which keeps it as the adapters change. Each token
/// is new to the watch: none is ever given out twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterSet {
    adapters: BTreeMap<String, AdapterState>,
    /// The unique name of the bluetoothd the adapters were read from, `None`
    /// while none answered. A restarted one has a new one.
    owner: Option<String>,
}

/// One adapter in an [`AdapterSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterState {
    /// What was registered with the adapter - a battery provider - stands
    /// while this is unchanged. It changes when an adapter is new at the
    /// path, or bluetoothd is.
    pub registration: u64,
    /// `Some` while the adapter is powered. Discovery started there stands
    /// while this is unchanged. It changes each time the adapter powers on,
    /// as powering off ended it, and with `registration`.
    pub discovery: Option<u64>,
}

impl AdapterSet {
    /// The adapter whose object is at `path`.
    pub fn get(&self, path: &str) -> Option<&AdapterState> {
        self.adapters.get(path)
    }

    /// Every adapter, by path, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AdapterState)> {
        self.adapters
            .iter()
            .map(|(path, adapter)| (path.as_str(), adapter))
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Whether BlueZ is on the bus, as far as the watch knows: once it has
    /// left, until it is read again.
    pub fn on_bus(&self) -> bool {
        self.owner.is_some()
    }

    /// The set as read from the bluetoothd at `owner`: `adapters`, each new
    /// unless that is the bluetoothd this set was read from and the adapter
    /// was here all along. `changes` are what each sender signalled since
    /// the last time, by sender: that bluetoothd's tell what it ended in
    /// between - see [`AdapterChange`] - and the rest are not its.
    ///
    /// Read twice from one bluetoothd - its leaving and coming back seen
    /// apart, the first read answered by the new one already - the second
    /// changes nothing. Given new tokens, attachment registered its
    /// providers again, BlueZ refused each as registered already, and GNOME
    /// Settings lost the batteries.
    fn read(
        &self,
        adapters: Vec<Adapter>,
        owner: String,
        mut changes: BySender,
        tokens: &mut Tokens,
    ) -> Self {
        let changes = changes.remove(&owner).unwrap_or_default();
        let same = self.owner.as_ref() == Some(&owner);
        let adapters = adapters
            .into_iter()
            .map(|adapter| {
                let change = changes.get(&adapter.path);
                let kept = self
                    .adapters
                    .get(&adapter.path)
                    .filter(|_| same && !change.is_some_and(|c| c.removed));
                let state = AdapterState {
                    registration: kept.map_or_else(|| tokens.next(), |k| k.registration),
                    discovery: adapter
                        .powered
                        .then(|| match kept.and_then(|k| k.discovery) {
                            Some(token) if !change.is_some_and(|c| c.off) => token,
                            _ => tokens.next(),
                        }),
                };
                (adapter.path, state)
            })
            .collect();
        Self {
            adapters,
            owner: Some(owner),
        }
    }

    /// Takes in what each adapter's signals said since the last time - see
    /// [`AdapterChange`] - from the bluetoothd this set was read from. What
    /// any other sender said is stale - from one that has left, taken out of
    /// order - or not bluetoothd's at all, and is left out; while none was
    /// read from, everything is.
    fn apply(&mut self, mut changes: BySender, tokens: &mut Tokens) {
        let Some(owner) = &self.owner else {
            return;
        };
        let changes = changes.remove(owner).unwrap_or_default();
        for (path, change) in changes {
            let now = change.now.filter(|_| !change.removed || change.added);
            let Some(powered) = now else {
                self.adapters.remove(&path);
                continue;
            };
            // Removed in between, the one there now is another.
            let before = self
                .adapters
                .get(&path)
                .filter(|_| !change.removed)
                .copied();
            // Only an adapter announced comes in: a change of power to one
            // never announced is stale, or not bluetoothd's.
            if before.is_none() && !change.added {
                continue;
            }
            let registration = before.map_or_else(|| tokens.next(), |b| b.registration);
            let discovery = powered.then(|| match before.and_then(|b| b.discovery) {
                Some(token) if !change.off => token,
                _ => tokens.next(),
            });
            let state = AdapterState {
                registration,
                discovery,
            };
            self.adapters.insert(path, state);
        }
    }
}

/// Where an [`AdapterSet`]'s tokens come from.
#[derive(Debug, Default)]
struct Tokens(u64);

impl Tokens {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Watches BlueZ's adapters on `conn` (#50). Back once they have been read,
/// with the set as it is then - empty when BlueZ is not on the bus - and from
/// there on kept as the adapters power on and off, come and go, and
/// bluetoothd restarts: see [`AdapterSet`].
///
/// The receiver gets the latest set, never a queue of them, so a flapping
/// adapter is one change waiting however often it flaps, and it gets each
/// set once. The signals are read as they come, in a task of their own, and
/// coalesced - see [`AdapterChanges`] - while another task answers them: an
/// answer may need BlueZ asked, and the subscription takes in every signal
/// bluetoothd sends, a crowd's advertisements among them, which left unread
/// would stop zbus reading the connection, and the answer with it (#90).
///
/// Only the bluetoothd the set was read from changes it: a signal from any
/// other sender is stale - from one that has left, taken out of order - or
/// not bluetoothd's at all, as zbus cannot check a well-known sender.
///
/// The watch ends - `changed` fails - when the connection's signals do. Its
/// tasks end with it, or once every receiver is dropped.
pub async fn watch_adapters(conn: &Connection) -> Result<watch::Receiver<AdapterSet>> {
    // Subscribed before the adapters are read, so no change falls between.
    let changes = adapter_changes(conn).await?;
    let mut tokens = Tokens::default();
    let first = read_adapters(conn, &AdapterSet::default(), BTreeMap::new(), &mut tokens).await;
    let (sender, receiver) = watch::channel(first);
    let conn = conn.clone();
    tokio::spawn(async move {
        tokio::pin!(changes);
        loop {
            let changes = tokio::select! {
                () = sender.closed() => return,
                changes = changes.next() => match changes {
                    Some(changes) => changes,
                    None => return,
                },
            };
            let mut set = sender.borrow().clone();
            // bluetoothd came or went: read again, from whichever answers -
            // keeping the tokens if that is the one read from before. What it
            // signalled meanwhile is in what is read, but for what it ended
            // on the way, which the signals say.
            if changes.owner {
                set = read_adapters(&conn, &set, changes.adapters, &mut tokens).await;
            } else {
                set.apply(changes.adapters, &mut tokens);
            }
            sender.send_if_modified(|current| {
                let changed = *current != set;
                *current = set;
                changed
            });
        }
    });
    Ok(receiver)
}

/// BlueZ's adapters on `conn`, as read anew by `before` - see
/// [`AdapterSet::read`] - with `changes` what each sender signalled since.
/// None when BlueZ is not on the bus.
async fn read_adapters(
    conn: &Connection,
    before: &AdapterSet,
    changes: BySender,
    tokens: &mut Tokens,
) -> AdapterSet {
    match managed_objects_and_owner(conn).await {
        Ok((objects, owner)) => before.read(adapters_in(objects), owner, changes, tokens),
        Err(e) => {
            tracing::debug!("no Bluetooth adapters to read: {e:#}");
            AdapterSet::default()
        }
    }
}

/// Every object of whichever BlueZ answers on `conn`, and the unique name of
/// the one that did: its reply's sender, so the two cannot be from two
/// bluetoothds.
async fn managed_objects_and_owner(conn: &Connection) -> Result<(ManagedObjects, String)> {
    let reply = conn
        .call_method(
            Some(BLUEZ_SERVICE),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .context("failed to get managed objects")?;
    let owner = reply
        .header()
        .sender()
        .context("BlueZ's reply names no sender")?
        .to_string();
    let objects = reply
        .body()
        .deserialize::<ManagedObjects>()
        .context("failed to read managed objects")?;
    Ok((objects, owner))
}

/// What changes BlueZ's adapters, as it is signalled, coalesced since it was
/// last taken - see [`watch_adapters`].
async fn adapter_changes(conn: &Connection) -> Result<impl Stream<Item = AdapterChanges> + use<>> {
    // Two rules, as `NameOwnerChanged` comes from the bus, not from BlueZ.
    // The bus's first: it brings so few that none waits long while the bus
    // confirms the second. The second takes in every signal bluetoothd
    // sends - `InterfacesAdded` from its ObjectManager, `PropertiesChanged`
    // from each adapter - in one stream, which keeps them in the order they
    // were sent: an adapter's `Powered` comes after the `InterfacesAdded`
    // that announces it.
    let owner = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, BLUEZ_SERVICE)?
        .build();
    let owner = MessageStream::for_match_rule(owner, conn, None)
        .await
        .context("failed to subscribe to BlueZ's name")?;
    let bluez = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BLUEZ_SERVICE)?
        .build();
    let bluez = MessageStream::for_match_rule(bluez, conn, None)
        .await
        .context("failed to subscribe to BlueZ's signals")?;
    let signals = futures_util::stream::select(owner, bluez)
        .filter_map(|msg| async move { adapter_signal(&msg.ok()?) });
    Ok(drain::<AdapterChanges, _>(signals))
}

/// A signal that changes BlueZ's adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdapterSignal {
    /// `org.bluez` changed hands: bluetoothd left the bus, or came on it.
    Owner,
    /// One adapter's, from the unique name `sender`.
    Adapter {
        sender: String,
        path: String,
        event: AdapterEvent,
    },
}

/// What happened to one adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterEvent {
    /// It appeared, powered or not.
    Added {
        powered: bool,
    },
    Powered(bool),
    Removed,
}

/// The [`AdapterSignal`] `msg` is, if it is one. zbus cannot check a
/// well-known sender, so whatever signal the connection gets comes through
/// here: what is not an adapter's goes by its headers wherever it can, before
/// its body is read. Which sender an adapter's is from is the watch's to
/// judge; `NameOwnerChanged` counts only from the bus itself.
fn adapter_signal(msg: &Message) -> Option<AdapterSignal> {
    let header = msg.header();
    let sender = header.sender()?.as_str();
    let adapter = |path: &str, event| AdapterSignal::Adapter {
        sender: sender.to_string(),
        path: path.to_string(),
        event,
    };
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus", "NameOwnerChanged") => {
            if sender != "org.freedesktop.DBus" {
                return None;
            }
            let (name, _, _) = msg.body().deserialize::<(String, String, String)>().ok()?;
            (name == BLUEZ_SERVICE).then_some(AdapterSignal::Owner)
        }
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            // BlueZ keeps its adapters at `/org/bluez/<name>`, and their
            // devices under them: a device's changes, one per advertisement,
            // go here.
            let path = header.path()?.as_str();
            let name = path.strip_prefix("/org/bluez/")?;
            if name.contains('/') {
                return None;
            }
            let (iface, changed, _) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            if iface != ADAPTER_INTERFACE {
                return None;
            }
            let powered = bool::try_from(changed.get("Powered")?.clone()).ok()?;
            Some(adapter(path, AdapterEvent::Powered(powered)))
        }
        ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
            let (path, interfaces) = msg
                .body()
                .deserialize::<(
                    OwnedObjectPath,
                    HashMap<String, HashMap<String, OwnedValue>>,
                )>()
                .ok()?;
            let props = interfaces.get(ADAPTER_INTERFACE)?;
            let powered = bool_prop(props, "Powered");
            Some(adapter(path.as_str(), AdapterEvent::Added { powered }))
        }
        ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
            let (path, interfaces) = msg
                .body()
                .deserialize::<(OwnedObjectPath, Vec<String>)>()
                .ok()?;
            interfaces
                .iter()
                .any(|iface| iface == ADAPTER_INTERFACE)
                .then(|| adapter(path.as_str(), AdapterEvent::Removed))
        }
        _ => None,
    }
}

/// The adapter signals waiting for the watch, taken all at once: what each
/// adapter's said, by sender, and whether bluetoothd came or went, since they
/// were last taken.
///
/// An adapter flapping, or a burst of them, is one change per adapter
/// however long it goes on: its signals say where it ended up, and what it
/// went through on the way that a client has to start again after - see
/// [`AdapterChange`]. Queued one per signal, each would be answered in turn,
/// long after the adapter had settled (#96).
#[derive(Debug, Default, PartialEq, Eq)]
struct AdapterChanges {
    /// Whether bluetoothd left the bus or came on it.
    owner: bool,
    /// What each adapter's signals said, by sender, then by path.
    adapters: BySender,
}

/// What adapters' signals said, by the unique name that sent them, then by
/// the adapter's path.
type BySender = BTreeMap<String, BTreeMap<String, AdapterChange>>;

/// What one adapter's signals said since they were last taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdapterChange {
    /// Whether it is there now, and powered: `None` once removed.
    now: Option<bool>,
    /// Whether it was announced in between.
    added: bool,
    /// Whether it was removed in between: one there now is another.
    removed: bool,
    /// Whether it was off in between, which ended discovery there.
    off: bool,
}

impl Queue for AdapterChanges {
    type Item = AdapterSignal;
    type Out = AdapterChanges;

    fn push(&mut self, signal: AdapterSignal) {
        let (sender, path, event) = match signal {
            AdapterSignal::Owner => {
                self.owner = true;
                return;
            }
            AdapterSignal::Adapter {
                sender,
                path,
                event,
            } => (sender, path, event),
        };
        let now = match event {
            AdapterEvent::Added { powered } | AdapterEvent::Powered(powered) => Some(powered),
            AdapterEvent::Removed => None,
        };
        let change = self
            .adapters
            .entry(sender)
            .or_default()
            .entry(path)
            .or_insert(AdapterChange {
                now,
                added: false,
                removed: false,
                off: false,
            });
        change.now = now;
        change.added |= matches!(event, AdapterEvent::Added { .. });
        change.removed |= now.is_none();
        change.off |= now != Some(true);
    }

    fn pop(&mut self) -> Option<AdapterChanges> {
        (self.owner || !self.adapters.is_empty()).then(|| std::mem::take(self))
    }
}

/// The longest a task waits before it tries again - see [`backoff`].
pub const RETRY_MAX: Duration = Duration::from_secs(60);

/// How long to wait before trying again after `failures` failures in a row:
/// a second, doubling up to [`RETRY_MAX`].
pub fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(6);
    Duration::from_secs(1 << doublings).min(RETRY_MAX)
}

/// The waits of a background task that sets itself up again whenever it
/// stops - when the system bus cannot be reached, or BlueZ's signals on it
/// end: [`backoff`], counting a run that ended within [`RETRY_MAX`] as a
/// failure too, so that a connection failing at once is not tried in a loop.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// How long to wait after another failure.
    pub fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        backoff(self.failures)
    }

    /// How long to wait after a run of `ran` that ended. One that went on a
    /// while starts the waits from the shortest again.
    pub fn ended(&mut self, ran: Duration) -> Duration {
        if ran >= RETRY_MAX {
            self.failures = 0;
        }
        self.failed()
    }
}

/// BlueZ's device objects: which are connected AirPods, what each is called,
/// and the connection watch.
#[derive(Clone)]
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
        let objects = managed_objects(&self.conn).await?;
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

    /// A device's address, and the path of the adapter it is on - its
    /// `Adapter` - read through one proxy, so in one `GetAll`. Fails when the
    /// address cannot be read; the adapter alone failing is the inner error.
    pub async fn device_address_and_adapter(
        &self,
        device_path: &str,
    ) -> Result<(Address, Result<String>)> {
        let proxy = Device1Proxy::builder(&self.conn)
            .path(device_path.to_string())?
            .build()
            .await?;
        let address = proxy
            .address()
            .await
            .context("failed to get device address")?;
        let address = address
            .parse()
            .with_context(|| format!("invalid device address {address:?}"))?;
        let adapter = proxy
            .adapter()
            .await
            .map(|adapter| adapter.to_string())
            .context("failed to get device adapter");
        Ok((address, adapter))
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
        let objects = managed_objects(&self.conn).await?;
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
    /// the device is not recognizable yet when `Connected` changes.
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
/// address for the scanner, [`AdapterChanges`] for the adapter watch. Each
/// `pop` gives out an `Out`: one item, or all of them in one. Neither `push`
/// nor `pop` may panic halfway - see [`lock`].
pub(crate) trait Queue: Default + Send + 'static {
    type Item: Send + 'static;
    type Out: Send + 'static;
    fn push(&mut self, item: Self::Item);
    fn pop(&mut self) -> Option<Self::Out>;
}

/// Reads `events` as they come, in a task of its own, into a `Q`, from where
/// the returned stream takes them at its reader's pace. The task ends with
/// `events`, or once the stream is dropped; the stream ends once the task has
/// and nothing is pending.
pub(crate) fn drain<Q, S>(events: S) -> impl Stream<Item = Q::Out>
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
    type Out = ConnectionEvent;

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

    /// One object per device, under its adapter's root, and a MAC's colons
    /// are not legal in an object path.
    #[test]
    fn battery_paths_are_per_device_adapter_and_connection() {
        let root = provider_root(":1.42", "/org/bluez/hci1").unwrap();
        let app = crate::APP_PATH;
        assert_eq!(root.as_str(), format!("{app}/battery/_1_42/hci1"));
        assert_eq!(
            battery_path(&root, MAC_A).unwrap().as_str(),
            format!("{app}/battery/_1_42/hci1/airpods_battery_AA_BB_CC_DD_EE_FF")
        );
        assert_ne!(
            battery_path(&root, MAC_A).unwrap(),
            battery_path(&root, MAC_B).unwrap()
        );
        assert_ne!(provider_root(":1.42", "/org/bluez/hci0").unwrap(), root);
        assert_ne!(
            provider_root(":1.43", "/org/bluez/hci1").unwrap(),
            root,
            "another app's"
        );
        assert!(provider_root(":1.42", "/").is_err(), "names no adapter");
    }

    /// A GetManagedObjects reply, of `objects` each with `interfaces`.
    fn managed<'a>(
        objects: impl IntoIterator<Item = (&'a str, Vec<(&'a str, HashMap<String, OwnedValue>)>)>,
    ) -> ManagedObjects {
        objects
            .into_iter()
            .map(|(path, interfaces)| {
                let interfaces = interfaces
                    .into_iter()
                    .map(|(name, props)| (name.try_into().unwrap(), props))
                    .collect();
                (path.try_into().unwrap(), interfaces)
            })
            .collect()
    }

    /// Every object with `Adapter1` is an adapter, whatever it is called, and
    /// nothing else is: not a device, nor the root object.
    #[test]
    fn adapters_are_the_objects_with_adapter1() {
        let powered = |on: bool| device([("Powered", Value::from(on))]);
        let objects = managed([
            ("/org/bluez", vec![("org.bluez.AgentManager1", device([]))]),
            (
                "/org/bluez/hci1",
                vec![
                    (ADAPTER_INTERFACE, powered(true)),
                    ("org.bluez.BatteryProviderManager1", device([])),
                ],
            ),
            (
                "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF",
                vec![(
                    "org.bluez.Device1",
                    device([("Connected", Value::from(true))]),
                )],
            ),
            ("/org/bluez/hci0", vec![(ADAPTER_INTERFACE, powered(false))]),
            ("/org/bluez/hci2", vec![(ADAPTER_INTERFACE, device([]))]),
        ]);
        let adapter = |path: &str, powered| Adapter {
            path: path.to_string(),
            powered,
        };
        assert_eq!(
            adapters_in(objects),
            [
                adapter("/org/bluez/hci0", false),
                adapter("/org/bluez/hci1", true),
                adapter("/org/bluez/hci2", false),
            ]
        );
        assert!(adapters_in(ManagedObjects::new()).is_empty());
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

    /// A rename leaves the UUIDs alone, so the pair is still recognized.
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

    const HCI0: &str = "/org/bluez/hci0";
    const HCI1: &str = "/org/bluez/hci1";
    /// The unique names of two bluetoothds, one after the other.
    const OLD: &str = ":1.5";
    const NEW: &str = ":1.9";

    fn signal(sender: &str, path: &str, event: AdapterEvent) -> AdapterSignal {
        AdapterSignal::Adapter {
            sender: sender.to_string(),
            path: path.to_string(),
            event,
        }
    }

    fn added(path: &str, powered: bool) -> AdapterSignal {
        signal(OLD, path, AdapterEvent::Added { powered })
    }

    fn powered(path: &str, powered: bool) -> AdapterSignal {
        signal(OLD, path, AdapterEvent::Powered(powered))
    }

    fn removed(path: &str) -> AdapterSignal {
        signal(OLD, path, AdapterEvent::Removed)
    }

    /// The changes `signals` make, as the watch takes them.
    fn changes(signals: impl IntoIterator<Item = AdapterSignal>) -> AdapterChanges {
        let mut changes = AdapterChanges::default();
        for signal in signals {
            changes.push(signal);
        }
        let taken = changes.pop().unwrap_or_default();
        assert_eq!(changes.pop(), None, "all taken at once");
        taken
    }

    /// What `signals` say of each adapter, by sender.
    fn by_sender(signals: impl IntoIterator<Item = AdapterSignal>) -> BySender {
        changes(signals).adapters
    }

    fn adapters(adapters: &[(&str, bool)]) -> Vec<Adapter> {
        adapters
            .iter()
            .map(|&(path, powered)| Adapter {
                path: path.to_string(),
                powered,
            })
            .collect()
    }

    /// A set with `adapters`, each new, as first read from the bluetoothd at
    /// `owner`.
    fn read(owner: &str, list: &[(&str, bool)], tokens: &mut Tokens) -> AdapterSet {
        AdapterSet::default().read(adapters(list), owner.to_string(), BySender::new(), tokens)
    }

    /// An adapter flapping is one change however long it flaps, which says
    /// where it ended up and that it was off on the way - and each adapter's
    /// is its own, as is each sender's.
    #[test]
    fn a_flapping_adapter_is_one_change() {
        let mut flapping = Vec::new();
        for _ in 0..1000 {
            flapping.push(powered(HCI0, false));
            flapping.push(powered(HCI0, true));
        }
        flapping.push(powered(HCI1, true));
        flapping.push(signal(NEW, HCI1, AdapterEvent::Removed));
        let mut taken = changes(flapping);
        assert!(!taken.owner);
        let change = |now, removed, off| AdapterChange {
            now,
            added: false,
            removed,
            off,
        };
        assert_eq!(
            taken.adapters.remove(OLD).unwrap(),
            BTreeMap::from([
                (HCI0.to_string(), change(Some(true), false, true)),
                (HCI1.to_string(), change(Some(true), false, false)),
            ])
        );
        assert_eq!(
            taken.adapters.remove(NEW).unwrap(),
            BTreeMap::from([(HCI1.to_string(), change(None, true, true))])
        );
        assert!(changes([]).adapters.is_empty());
        assert!(changes([AdapterSignal::Owner, AdapterSignal::Owner]).owner);
    }

    /// Discovery has to start again on an adapter that was off at any point,
    /// and everything on one that went at any point; neither on one that
    /// stayed. One that went for good is gone from the set.
    #[test]
    fn tokens_change_with_what_bluez_forgot() {
        let mut tokens = Tokens::default();
        let mut set = read(OLD, &[(HCI0, true), (HCI1, false)], &mut tokens);
        assert!(set.on_bus());
        let before = *set.get(HCI0).unwrap();
        assert_eq!(set.get(HCI1).unwrap().discovery, None, "off");

        // Announced again, as it was: nothing to start again.
        let mut same = set.clone();
        same.apply(by_sender([added(HCI0, true)]), &mut tokens);
        assert_eq!(same, set);

        let mut cycled = set.clone();
        cycled.apply(
            by_sender([powered(HCI0, false), powered(HCI0, true)]),
            &mut tokens,
        );
        let after = *cycled.get(HCI0).unwrap();
        assert_eq!(after.registration, before.registration, "providers kept");
        assert_ne!(after.discovery, before.discovery, "discovery ended");
        assert!(after.discovery.is_some());

        let mut replugged = set.clone();
        replugged.apply(by_sender([removed(HCI0), added(HCI0, true)]), &mut tokens);
        let after = *replugged.get(HCI0).unwrap();
        assert_ne!(after.registration, before.registration);
        assert_ne!(after.discovery, before.discovery);

        set.apply(
            by_sender([removed(HCI1), powered(HCI0, false)]),
            &mut tokens,
        );
        assert_eq!(set.iter().map(|(path, _)| path).collect::<Vec<_>>(), [HCI0]);
        assert_eq!(set.get(HCI0).unwrap().discovery, None);
        assert!(!AdapterSet::default().on_bus());
    }

    /// Read again from the bluetoothd it was read from, the set keeps its
    /// tokens - but for what that bluetoothd ended meanwhile. Read from a new
    /// one, every token is new. Regression (rules review of #121): a restart
    /// seen as two changes of owner, the first read answered by the new
    /// bluetoothd already, gave new tokens twice; attachment registered its
    /// provider again, BlueZ refused it as registered already, and GNOME
    /// Settings lost the battery.
    #[test]
    fn reading_one_bluetoothd_again_keeps_the_tokens() {
        let mut tokens = Tokens::default();
        let both = [(HCI0, true), (HCI1, true)];
        let old = read(OLD, &both, &mut tokens);
        let new = old.read(
            adapters(&both),
            NEW.to_string(),
            BySender::new(),
            &mut tokens,
        );
        for path in [HCI0, HCI1] {
            let (was, now) = (old.get(path).unwrap(), new.get(path).unwrap());
            assert_ne!(now.registration, was.registration);
            assert_ne!(now.discovery, was.discovery);
        }

        // Off and on again meanwhile, as the new one signalled; the old one's
        // last word, out of order, counts for nothing.
        let meanwhile = by_sender([
            signal(NEW, HCI1, AdapterEvent::Powered(false)),
            signal(NEW, HCI1, AdapterEvent::Powered(true)),
            signal(OLD, HCI0, AdapterEvent::Removed),
        ]);
        let again = new.read(adapters(&both), NEW.to_string(), meanwhile, &mut tokens);
        assert_eq!(again.get(HCI0), new.get(HCI0), "read twice");
        let (was, now) = (new.get(HCI1).unwrap(), again.get(HCI1).unwrap());
        assert_eq!(now.registration, was.registration);
        assert_ne!(now.discovery, was.discovery, "it was off in between");
    }

    /// Only the bluetoothd the set was read from changes it. A change of
    /// power for an adapter it never announced is stale - its last before it
    /// left, taken after the read that found it gone - and is not taken in,
    /// and nor is anything from another sender: one that has left, or one
    /// that is not bluetoothd at all. While none was read from, nothing is.
    #[test]
    fn signals_from_anyone_but_the_bluetoothd_read_from_change_nothing() {
        let mut tokens = Tokens::default();
        let mut set = read(NEW, &[(HCI0, true)], &mut tokens);
        let before = set.clone();
        let from_new = |path, event| signal(NEW, path, event);
        set.apply(
            by_sender([from_new(HCI1, AdapterEvent::Powered(true))]),
            &mut tokens,
        );
        assert_eq!(set, before, "a power change for an adapter never announced");

        set.apply(
            by_sender([
                powered(HCI0, false),
                signal(":1.66", HCI1, AdapterEvent::Added { powered: true }),
            ]),
            &mut tokens,
        );
        assert_eq!(set, before, "from the old one, or a stranger");

        // Gone from the bus: the old one's shutdown, taken after the read
        // that found nobody, adds nothing.
        let mut away = AdapterSet::default();
        away.apply(
            by_sender([added(HCI0, true), powered(HCI0, false)]),
            &mut tokens,
        );
        assert!(away.is_empty() && !away.on_bus());

        // An announced one comes in, from the bluetoothd read from.
        set.apply(
            by_sender([from_new(HCI1, AdapterEvent::Added { powered: true })]),
            &mut tokens,
        );
        assert!(set.get(HCI1).is_some());
    }

    /// The signals that change the adapters are told from the rest - a
    /// device's, another name's, one claiming to be the bus's - and each says
    /// what it should, and who sent it.
    #[test]
    fn reads_the_signals_that_change_adapters() {
        let props = |powered: bool| HashMap::from([("Powered", Value::from(powered))]);
        let path = ObjectPath::try_from(HCI0).unwrap();
        let from = |builder: zbus::message::Builder<'static>| builder.sender(OLD).unwrap();
        let interfaces = HashMap::from([(ADAPTER_INTERFACE, props(true))]);
        let msg = from(
            Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded").unwrap(),
        )
        .build(&(&path, interfaces))
        .unwrap();
        assert_eq!(adapter_signal(&msg), Some(added(HCI0, true)));

        let msg = from(
            Message::signal(
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesRemoved",
            )
            .unwrap(),
        )
        .build(&(&path, vec![ADAPTER_INTERFACE]))
        .unwrap();
        assert_eq!(adapter_signal(&msg), Some(removed(HCI0)));

        let changed = |object: &'static str, iface: &str, props| {
            from(
                Message::signal(
                    object,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                )
                .unwrap(),
            )
            .build(&(iface, props, Vec::<&str>::new()))
            .unwrap()
        };
        let msg = changed(HCI0, ADAPTER_INTERFACE, props(false));
        assert_eq!(adapter_signal(&msg), Some(powered(HCI0, false)));
        let discovering = HashMap::from([("Discovering", Value::from(true))]);
        assert_eq!(
            adapter_signal(&changed(HCI0, ADAPTER_INTERFACE, discovering)),
            None
        );
        assert_eq!(
            adapter_signal(&changed(PATH_A, "org.bluez.Device1", props(false))),
            None,
            "a device's"
        );

        let owner = |sender: &str, name: &str| {
            Message::signal(
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameOwnerChanged",
            )
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(&(name, OLD, ""))
            .unwrap()
        };
        assert_eq!(
            adapter_signal(&owner("org.freedesktop.DBus", BLUEZ_SERVICE)),
            Some(AdapterSignal::Owner)
        );
        assert_eq!(
            adapter_signal(&owner("org.freedesktop.DBus", "org.example")),
            None
        );
        assert_eq!(
            adapter_signal(&owner(":1.66", BLUEZ_SERVICE)),
            None,
            "not the bus's"
        );

        let device = ObjectPath::try_from(PATH_A).unwrap();
        let interfaces = HashMap::from([("org.bluez.Device1", props(false))]);
        let msg = from(
            Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded").unwrap(),
        )
        .build(&(&device, interfaces))
        .unwrap();
        assert_eq!(adapter_signal(&msg), None, "a device appearing");
    }

    /// A second, doubling to a minute; a run that went on a while starts the
    /// waits over, one that ended at once does not.
    #[test]
    fn retries_back_off_to_a_minute() {
        let waits: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(backoff(u32::MAX), RETRY_MAX);

        let mut backoff = Backoff::default();
        assert_eq!(backoff.failed().as_secs(), 1);
        assert_eq!(backoff.ended(Duration::from_secs(1)).as_secs(), 2);
        assert_eq!(backoff.ended(RETRY_MAX).as_secs(), 1);
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
