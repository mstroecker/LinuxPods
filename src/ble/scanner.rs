//! BLE advertisement scanning via BlueZ D-Bus.
//!
//! Takes the Apple manufacturer data (company ID 0x004C) of each advertisement
//! BlueZ signals on org.bluez.Device1 and parses proximity pairing
//! advertisements out of it. An address heard for the first time becomes a
//! device, announced by the ObjectManager's InterfacesAdded with that first
//! advertisement among its properties; every later one comes as a
//! PropertiesChanged. Both are read: watching PropertiesChanged alone lost
//! the first advertisement from every address, and AirPods rotate theirs every
//! few seconds while disconnected (#86).
//!
//! Accuracy note carried over from the Go docs: cleartext advertisements give
//! battery in 10% steps. Decrypting with a stored ENC_KEY yields 1% accuracy.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream, proxy};

use super::parser::{ProximityData, parse_proximity_data};
use crate::bluez::BLUEZ_SERVICE;

/// The key of Apple's entry in a device's `ManufacturerData`.
pub const APPLE_COMPANY_ID: u16 = 0x004C;

const DEVICE_INTERFACE: &str = "org.bluez.Device1";

#[proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
}

/// One parsed advertisement plus the (randomized) MAC it arrived from.
#[derive(Debug, Clone)]
pub struct Advertisement {
    pub data: ProximityData,
    /// BLE MAC, which rotates periodically for privacy.
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

    /// Sets an LE-only discovery filter and starts scanning.
    pub async fn start_discovery(&self) -> Result<()> {
        let adapter = Adapter1Proxy::new(&self.conn).await?;

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

    pub async fn stop_discovery(&self) -> Result<()> {
        let adapter = Adapter1Proxy::new(&self.conn).await?;
        adapter
            .stop_discovery()
            .await
            .context("failed to stop discovery")
    }

    /// Yields advertisements as they arrive. Replaces the Go version's
    /// ScanForAirPods(timeout) polling call with a stream the coordinator drives.
    ///
    /// One stream for both signals, so reading it drains both subscriptions:
    /// one left unread would fill zbus's queue, and zbus would stop reading the
    /// connection. Merged as they arrive, an address's first advertisement may
    /// come out after its second, which the next one puts right.
    pub async fn advertisements(&self) -> Result<impl futures_util::Stream<Item = Advertisement>> {
        let changed = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace("/org/bluez")?
            .build();
        // Sent from bluetoothd's ObjectManager at `/`, so the sender is what
        // tells it from another service's.
        let added = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)?
            .interface("org.freedesktop.DBus.ObjectManager")?
            .member("InterfacesAdded")?
            .build();

        let changed = MessageStream::for_match_rule(changed, &self.conn, None)
            .await
            .context("failed to subscribe to PropertiesChanged")?;
        let added = MessageStream::for_match_rule(added, &self.conn, None)
            .await
            .context("failed to subscribe to InterfacesAdded")?;

        let signals = futures_util::stream::select(changed, added);
        Ok(signals.filter_map(|msg| async move {
            let msg = msg.ok()?;
            let (path, properties) = device_properties(&msg)?;
            let apple_data = apple_manufacturer_data(&properties)?;
            let Some(ble_mac) = mac_from_path(&path) else {
                tracing::debug!("BLE advertisement dropped: {path} names no device address");
                return None;
            };

            // Logged here rather than downstream so it covers every advertisement
            // that parses, including ones the coordinator later drops (for example
            // the device currently on AAP).
            match parse_proximity_data(&apple_data) {
                Ok(data) => {
                    tracing::debug!(
                        "BLE parsable: {ble_mac} model=0x{:04X} payload={}B",
                        data.product_id,
                        data.raw_data.len()
                    );
                    // Every parsable advertisement, including the ones the
                    // coordinator drops as superseded by AAP - which is the only
                    // place to see the payload of a device connected to this host.
                    tracing::trace!(
                        "BLE parsable raw: {ble_mac} {}",
                        data.raw_data
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    Some(Advertisement { data, ble_mac })
                }
                Err(e) => {
                    // Apple broadcasts plenty of non-proximity message types; at
                    // debug level these would drown out everything else.
                    tracing::trace!("BLE unparsable from {ble_mac}: {e}");
                    None
                }
            }
        }))
    }
}

/// The path of the device a BlueZ signal is about, and the Device1 properties
/// it carries: all of them in the InterfacesAdded that announces a new
/// device, the changed ones in a PropertiesChanged. `None` for any other
/// signal.
///
/// InterfacesAdded comes from the ObjectManager and names the device in its
/// body; PropertiesChanged comes from the device itself.
fn device_properties(msg: &Message) -> Option<(String, HashMap<String, OwnedValue>)> {
    let header = msg.header();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path = header.path()?.to_string();
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
            // Under /org/bluez, as the PropertiesChanged rule has it.
            if !path.as_str().starts_with("/org/bluez/") {
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

/// /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF -> AA:BB:CC:DD:EE:FF
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
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_TOO_SHORT"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_GG"), None);
        assert_eq!(mac_from_path(""), None);
    }
}
