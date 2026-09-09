//! BlueZ BatteryProvider1 integration.
//!
//! Port of internal/bluez/battery_provider.go. The Go version hand-implemented
//! org.freedesktop.DBus.ObjectManager - GetManagedObjects, the introspection XML,
//! and manual InterfacesAdded / InterfacesRemoved / PropertiesChanged emission.
//! zbus ships `fdo::ObjectManager` and derives properties, so all of that is gone:
//! registering the interface at a child path emits InterfacesAdded on its own.

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, interface, proxy};

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

        Ok(Self { conn, battery_path: None })
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
                BatteryProvider1 { percentage, device, source: SOURCE.to_string() },
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
        proxy.address().await.context("failed to get device address")
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

/// An alias containing "AirPods" is how the Go version identified the device.
pub fn is_airpods(props: &HashMap<String, OwnedValue>) -> bool {
    props
        .get("Alias")
        .and_then(|v| String::try_from(v.clone()).ok())
        .map(|alias| alias.contains("AirPods"))
        .unwrap_or(false)
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

    #[test]
    fn identifies_airpods_by_alias() {
        assert!(is_airpods(&props(Some("Marcel's AirPods Pro"))));
        assert!(is_airpods(&props(Some("AirPods"))));
        assert!(!is_airpods(&props(Some("Sony WH-1000XM5"))));
        assert!(!is_airpods(&props(None)));
    }
}
