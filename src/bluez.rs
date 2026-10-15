//! BlueZ BatteryProvider1 integration.
//!
//! zbus ships `fdo::ObjectManager` and derives properties, so none of the usual
//! ObjectManager boilerplate is needed - no GetManagedObjects, no introspection
//! XML, no manual InterfacesAdded / InterfacesRemoved / PropertiesChanged:
//! registering the interface at a child path emits InterfacesAdded on its own.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream, interface, proxy};

const BLUEZ_SERVICE: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const PROVIDER_PATH: &str = "/com/github/mstroecker/linuxpods/battery";
const BATTERY_NAME: &str = "airpods_battery";
const SOURCE: &str = "LinuxPods";

/// The service UUID every AirPods lists in `Device1.UUIDs` - the identifier
/// LibrePods uses too. Unlike the alias it survives a rename, and unlike Apple's
/// vendor ID in `Modalias` it does not also match Apple keyboards and mice.
const AIRPODS_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

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

/// Owns the D-Bus connection and the registered provider.
pub struct BatteryProvider {
    conn: Connection,
    /// The level each exported battery object shows, keyed by MAC. An attached
    /// device has one only while it has a reading - see [`battery_actions`].
    batteries: HashMap<String, u8>,
}

/// Where one device's battery object lives, under the provider root.
fn battery_path(mac: &str) -> Result<OwnedObjectPath> {
    let path = format!("{PROVIDER_PATH}/{BATTERY_NAME}_{}", mac.replace(':', "_"));
    OwnedObjectPath::try_from(path.as_str()).with_context(|| format!("invalid battery path {path}"))
}

/// One change to the exported battery objects - see [`battery_actions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatteryAction {
    /// Export a device's object, starting at its first reading.
    Add {
        mac: String,
        device_path: String,
        percentage: u8,
    },
    Update {
        mac: String,
        percentage: u8,
    },
    /// Withdraw a device's object: its reading went, or the device did.
    Remove {
        mac: String,
    },
}

impl BatteryAction {
    fn mac(&self) -> &str {
        match self {
            Self::Add { mac, .. } | Self::Update { mac, .. } | Self::Remove { mac } => mac,
        }
    }
}

/// Decides how the exported battery objects change so that GNOME Settings shows
/// every attached device's reading, and nothing for one without a reading.
///
/// `Battery1` has no "unknown". An object exported ahead of the first reading sat
/// at 0% - a real level, so GNOME Settings and anything else reading BlueZ
/// batteries saw a flat battery, for the whole connection if neither AAP nor a
/// decryptable advertisement came through - and one that outlived its reading kept
/// showing a stale level. Absence is the only honest way to say "unknown".
///
/// `attached` maps BlueZ device paths to MACs. `levels` holds the reading of each
/// device that has one, `exported` the level each exported object shows, both
/// keyed by MAC. Sorted by MAC, so the order does not depend on the maps'.
pub fn battery_actions(
    attached: &HashMap<String, String>,
    levels: &HashMap<String, u8>,
    exported: &HashMap<String, u8>,
) -> Vec<BatteryAction> {
    let mut actions: Vec<BatteryAction> = attached
        .iter()
        .filter_map(
            |(device_path, mac)| match (levels.get(mac), exported.get(mac)) {
                (Some(&percentage), None) => Some(BatteryAction::Add {
                    mac: mac.clone(),
                    device_path: device_path.clone(),
                    percentage,
                }),
                // An unchanged level would only be signal traffic.
                (Some(&percentage), Some(&shown)) if percentage != shown => {
                    Some(BatteryAction::Update {
                        mac: mac.clone(),
                        percentage,
                    })
                }
                (None, Some(_)) => Some(BatteryAction::Remove { mac: mac.clone() }),
                _ => None,
            },
        )
        .chain(
            exported
                .keys()
                .filter(|mac| !attached.values().any(|m| m == *mac))
                .map(|mac| BatteryAction::Remove { mac: mac.clone() }),
        )
        .collect();
    actions.sort_by(|a, b| a.mac().cmp(b.mac()));
    actions
}

impl BatteryProvider {
    /// Connects to the system bus, serves the ObjectManager, and registers with BlueZ.
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("failed to connect to system bus")?;

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

        Ok(Self {
            conn,
            batteries: HashMap::new(),
        })
    }

    /// The level each exported battery object shows, keyed by MAC.
    pub fn exported(&self) -> &HashMap<String, u8> {
        &self.batteries
    }

    /// Carries out one of [`battery_actions`]' decisions.
    pub async fn apply(&mut self, action: &BatteryAction) -> Result<()> {
        match action {
            BatteryAction::Add {
                mac,
                device_path,
                percentage,
            } => self.add_battery(mac, device_path, *percentage).await,
            BatteryAction::Update { mac, percentage } => {
                self.update_percentage(mac, *percentage).await
            }
            BatteryAction::Remove { mac } => self.remove_battery(mac).await,
        }
    }

    /// Exports one device's battery object at its first reading. Registering the
    /// interface emits InterfacesAdded.
    async fn add_battery(&mut self, mac: &str, device_path: &str, percentage: u8) -> Result<()> {
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

        self.batteries.insert(mac.to_string(), percentage);
        Ok(())
    }

    /// Updates one device's percentage, emitting PropertiesChanged.
    async fn update_percentage(&mut self, mac: &str, percentage: u8) -> Result<()> {
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

        self.batteries.insert(mac.to_string(), percentage);
        Ok(())
    }

    /// Removes one device's battery object, emitting InterfacesRemoved.
    async fn remove_battery(&mut self, mac: &str) -> Result<()> {
        if self.batteries.remove(mac).is_some() {
            self.conn
                .object_server()
                .remove::<BatteryProvider1, _>(&battery_path(mac)?)
                .await
                .context("failed to remove battery object")?;
        }
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
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

    pub async fn device_address(&self, device_path: &str) -> Result<String> {
        let proxy = Device1Proxy::builder(&self.conn)
            .path(device_path.to_string())?
            .build()
            .await?;
        proxy
            .address()
            .await
            .context("failed to get device address")
    }

    /// Every paired device's BlueZ alias, keyed by uppercase MAC.
    ///
    /// The alias is the name the user sees everywhere else on the desktop - their
    /// own rename if they made one, the device's advertised name otherwise - so it
    /// is what the device switcher should show. Addresses are uppercased because
    /// that is how they arrive from `Device1.Address` and how the keystore keys
    /// its entries; a mismatch would silently fall back to the raw MAC.
    pub async fn device_aliases(&self) -> HashMap<String, String> {
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

    async fn read_device_aliases(&self) -> Result<HashMap<String, String>> {
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

    /// Unregisters the provider. Called on shutdown.
    pub async fn close(&mut self) -> Result<()> {
        let macs: Vec<String> = self.batteries.keys().cloned().collect();
        for mac in macs {
            let _ = self.remove_battery(&mac).await;
        }
        let manager = BatteryProviderManager1Proxy::new(&self.conn).await?;
        let path = ObjectPath::try_from(PROVIDER_PATH)?;
        manager.unregister_battery_provider(&path).await?;
        Ok(())
    }
}

/// A device connecting or disconnecting, or its services marking it as AirPods.
#[derive(Debug, Clone)]
pub struct ConnectionEvent {
    pub device_path: String,
    /// `None` when the services changed, not the connection: a first pairing
    /// connects before service discovery fills in the UUID that marks AirPods, so
    /// the device is not recognisable yet when `Connected` changes.
    pub connected: Option<bool>,
}

impl BatteryProvider {
    /// Stream of Connected changes on org.bluez.Device1, and of UUIDs changes
    /// that mark a device as AirPods.
    ///
    /// Port of the Go WatchForAirPods signal loop. Without this the provider only
    /// ever saw devices that were already connected at startup, so plugging the
    /// AirPods in later did nothing.
    pub async fn watch_connections(
        &self,
    ) -> Result<impl futures_util::Stream<Item = ConnectionEvent> + use<>> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace("/org/bluez")?
            .build();

        let stream = MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .context("failed to subscribe to device PropertiesChanged")?;

        Ok(stream.filter_map(|msg| async move {
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
        }))
    }
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

/// Pulls the (uppercase MAC, alias) pair out of a paired device's Device1 property map.
///
/// Both properties are required: an address with no alias has no name to show, and
/// an alias with no address cannot be matched to a device we track. Unpaired devices
/// are skipped: while we scan, BlueZ keeps an object for every advertiser nearby,
/// mostly random addresses named after themselves, and the map goes into every
/// snapshot - unfiltered, a BLE spam flood would grow it. The switcher lists only
/// devices whose key we hold, and getting a key takes a pairing.
fn device_name_entry(props: &HashMap<String, OwnedValue>) -> Option<(String, String)> {
    if !bool_prop(props, "Paired") {
        return None;
    }
    let string_prop = |k: &str| {
        props
            .get(k)
            .and_then(|v| String::try_from(v.clone()).ok())
            .filter(|s| !s.is_empty())
    };
    Some((
        string_prop("Address")?.to_uppercase(),
        string_prop("Alias")?,
    ))
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

    /// Both properties are needed, and the address is normalized because the
    /// keystore keys its entries on the uppercase form.
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
            Some((
                "AA:BB:CC:DD:EE:FF".to_string(),
                "Marcel's AirPods Pro".to_string()
            ))
        );
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
                "DE:AD:BE:EF:00:01".to_string(),
                "DE-AD-BE-EF-00-01".to_string()
            ))
        );
    }

    /// One object per device, and a MAC's colons are not legal in an object path.
    #[test]
    fn battery_paths_are_per_device() {
        assert_eq!(
            battery_path("AA:BB:CC:DD:EE:FF").unwrap().as_str(),
            "/com/github/mstroecker/linuxpods/battery/airpods_battery_AA_BB_CC_DD_EE_FF"
        );
        assert_ne!(
            battery_path("AA:BB:CC:DD:EE:FF").unwrap(),
            battery_path("11:22:33:44:55:66").unwrap()
        );
    }

    const MAC_A: &str = "AA:BB:CC:DD:EE:FF";
    const PATH_A: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
    const MAC_B: &str = "11:22:33:44:55:66";
    const PATH_B: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";

    fn map<V: Clone>(entries: &[(&str, V)]) -> HashMap<String, V> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    /// Pair A attached, as `bluez_task` keeps it: by device path, to its MAC.
    fn attached_a() -> HashMap<String, String> {
        map(&[(PATH_A, MAC_A.to_string())])
    }

    /// Nothing is exported until the first reading, and then at that level -
    /// never at a placeholder 0% ahead of it.
    #[test]
    fn first_reading_adds_the_battery_at_that_level() {
        assert!(battery_actions(&attached_a(), &map(&[]), &map(&[])).is_empty());
        assert_eq!(
            battery_actions(&attached_a(), &map(&[(MAC_A, 73)]), &map(&[])),
            [BatteryAction::Add {
                mac: MAC_A.to_string(),
                device_path: PATH_A.to_string(),
                percentage: 73,
            }]
        );
    }

    /// A snapshot that leaves the level alone sends no PropertiesChanged.
    #[test]
    fn unchanged_level_is_left_alone() {
        let levels = map(&[(MAC_A, 73)]);
        assert!(battery_actions(&attached_a(), &levels, &map(&[(MAC_A, 73)])).is_empty());
    }

    #[test]
    fn changed_level_updates_the_battery() {
        assert_eq!(
            battery_actions(&attached_a(), &map(&[(MAC_A, 72)]), &map(&[(MAC_A, 73)])),
            [BatteryAction::Update {
                mac: MAC_A.to_string(),
                percentage: 72,
            }]
        );
    }

    /// The link dropped or the BLE reading expired, with the pair still
    /// connected: the last level would be stale, and there is no "unknown".
    #[test]
    fn reading_gone_withdraws_the_battery() {
        assert_eq!(
            battery_actions(&attached_a(), &map(&[]), &map(&[(MAC_A, 73)])),
            [BatteryAction::Remove {
                mac: MAC_A.to_string()
            }]
        );
    }

    /// A disconnected pair loses its battery, though BLE may still hold a
    /// reading for it.
    #[test]
    fn detached_device_loses_its_battery() {
        assert_eq!(
            battery_actions(&map(&[]), &map(&[(MAC_A, 73)]), &map(&[(MAC_A, 73)])),
            [BatteryAction::Remove {
                mac: MAC_A.to_string()
            }]
        );
    }

    /// Each pair is decided on its own, and the order is by MAC.
    #[test]
    fn pairs_are_decided_independently() {
        let attached = map(&[(PATH_A, MAC_A.to_string()), (PATH_B, MAC_B.to_string())]);
        assert_eq!(
            battery_actions(
                &attached,
                &map(&[(MAC_A, 50)]),
                &map(&[(MAC_A, 60), (MAC_B, 40)])
            ),
            [
                BatteryAction::Remove {
                    mac: MAC_B.to_string()
                },
                BatteryAction::Update {
                    mac: MAC_A.to_string(),
                    percentage: 50,
                },
            ]
        );
    }

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
}
