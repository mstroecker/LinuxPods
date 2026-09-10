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
    battery_path: Option<OwnedObjectPath>,
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
            battery_path: None,
        })
    }

    /// Adds the battery object. Registering the interface emits InterfacesAdded.
    pub async fn add_battery(&mut self, percentage: u8, device_path: &str) -> Result<()> {
        let battery_path = OwnedObjectPath::try_from(format!("{PROVIDER_PATH}/{BATTERY_NAME}"))?;
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

        self.battery_path = Some(battery_path);
        Ok(())
    }

    /// Updates the percentage, emitting PropertiesChanged.
    pub async fn update_percentage(&self, percentage: u8) -> Result<()> {
        let Some(path) = &self.battery_path else {
            anyhow::bail!("battery device {BATTERY_NAME} not registered");
        };

        let iface_ref = self
            .conn
            .object_server()
            .interface::<_, BatteryProvider1>(path)
            .await
            .context("battery interface not found")?;

        let mut iface = iface_ref.get_mut().await;
        if iface.percentage == percentage {
            return Ok(()); // avoid pointless signal traffic
        }
        iface.percentage = percentage;
        iface
            .percentage_changed(iface_ref.signal_emitter())
            .await
            .context("failed to emit PropertiesChanged")?;
        Ok(())
    }

    /// Removes the battery object, emitting InterfacesRemoved.
    pub async fn remove_battery(&mut self) -> Result<()> {
        if let Some(path) = self.battery_path.take() {
            self.conn
                .object_server()
                .remove::<BatteryProvider1, _>(&path)
                .await
                .context("failed to remove battery object")?;
        }
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Finds a connected device whose alias contains "AirPods".
    pub async fn discover_airpods(&self) -> Result<String> {
        let om = zbus::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination(BLUEZ_SERVICE)?
            .path("/")?
            .build()
            .await?;

        let objects = om
            .get_managed_objects()
            .await
            .context("failed to get managed objects")?;

        for (path, interfaces) in objects {
            let Some(props) = interfaces.get("org.bluez.Device1") else {
                continue;
            };
            if !is_airpods(props) {
                continue;
            }
            let connected = props
                .get("Connected")
                .and_then(|v| bool::try_from(v.clone()).ok())
                .unwrap_or(false);
            if connected {
                return Ok(path.to_string());
            }
        }

        anyhow::bail!("no connected AirPods device found")
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

    /// Every known device's BlueZ alias, keyed by uppercase MAC.
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
        let Ok(builder) = Device1Proxy::builder(&self.conn).path(device_path.to_string()) else {
            return String::new();
        };
        match builder.build().await {
            Ok(proxy) => proxy.alias().await.unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// Unregisters the provider. Called on shutdown.
    pub async fn close(&mut self) -> Result<()> {
        let _ = self.remove_battery().await;
        let manager = BatteryProviderManager1Proxy::new(&self.conn).await?;
        let path = ObjectPath::try_from(PROVIDER_PATH)?;
        manager.unregister_battery_provider(&path).await?;
        Ok(())
    }
}

/// A device connecting or disconnecting.
#[derive(Debug, Clone)]
pub struct ConnectionEvent {
    pub device_path: String,
    pub connected: bool,
}

impl BatteryProvider {
    /// Stream of Connected changes on org.bluez.Device1.
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

            let connected = bool::try_from(changed.get("Connected")?.clone()).ok()?;
            Some(ConnectionEvent {
                device_path,
                connected,
            })
        }))
    }

    pub fn has_battery(&self) -> bool {
        self.battery_path.is_some()
    }
}

/// An alias containing "AirPods" is how the Go version identified the device.
pub fn is_airpods(props: &HashMap<String, OwnedValue>) -> bool {
    props
        .get("Alias")
        .and_then(|v| String::try_from(v.clone()).ok())
        .map(|alias| alias.contains("AirPods"))
        .unwrap_or(false)
}

/// Pulls the (uppercase MAC, alias) pair out of a Device1 property map.
///
/// Both properties are required: an address with no alias has no name to show, and
/// an alias with no address cannot be matched to a device we track.
fn device_name_entry(props: &HashMap<String, OwnedValue>) -> Option<(String, String)> {
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

    fn props(alias: Option<&str>) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        if let Some(a) = alias {
            m.insert(
                "Alias".to_string(),
                OwnedValue::try_from(zbus::zvariant::Value::from(a.to_string())).unwrap(),
            );
        }
        m
    }

    /// Both properties are needed, and the address is normalized because the
    /// keystore keys its entries on the uppercase form.
    #[test]
    fn name_entry_needs_an_address_and_an_alias() {
        let mut p = props(Some("Marcel's AirPods Pro"));
        assert_eq!(device_name_entry(&p), None);

        p.insert(
            "Address".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::from("aa:bb:cc:dd:ee:ff")).unwrap(),
        );
        assert_eq!(
            device_name_entry(&p),
            Some((
                "AA:BB:CC:DD:EE:FF".to_string(),
                "Marcel's AirPods Pro".to_string()
            ))
        );

        assert_eq!(device_name_entry(&props(None)), None);
    }

    #[test]
    fn identifies_airpods_by_alias() {
        assert!(is_airpods(&props(Some("Marcel's AirPods Pro"))));
        assert!(is_airpods(&props(Some("AirPods"))));
        assert!(!is_airpods(&props(Some("Sony WH-1000XM5"))));
        assert!(!is_airpods(&props(None)));
    }
}
