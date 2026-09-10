//! BLE advertisement scanning via BlueZ D-Bus.
//!
//! Watches PropertiesChanged on org.bluez.Device1
//! for Apple manufacturer data (company ID 0x004C) and parses proximity pairing
//! advertisements out of it.
//!
//! Accuracy note carried over from the Go docs: cleartext advertisements give
//! battery in 10% steps. Decrypting with a stored ENC_KEY yields 1% accuracy.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, proxy};

use super::parser::{ProximityData, parse_proximity_data};

const APPLE_COMPANY_ID: u16 = 0x004C;

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
    pub ble_mac: String,
}

pub struct Scanner {
    conn: Connection,
}

impl Scanner {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("failed to connect to system bus")?;
        Ok(Self { conn })
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
    pub async fn advertisements(&self) -> Result<impl futures_util::Stream<Item = Advertisement>> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace("/org/bluez")?
            .build();

        let stream = MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .context("failed to subscribe to PropertiesChanged")?;

        Ok(stream.filter_map(|msg| async move {
            let msg = msg.ok()?;
            let path = msg.header().path()?.to_string();

            let (iface, changed, _invalidated) = msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            if iface != "org.bluez.Device1" {
                return None;
            }

            let apple_data = apple_manufacturer_data(&changed)?;
            let ble_mac = mac_from_path(&path)?;

            // Logged here rather than downstream so it covers every advertisement
            // that parses, including ones the coordinator later drops (for example
            // the device currently on AAP).
            match parse_proximity_data(&apple_data) {
                Ok(data) => {
                    tracing::debug!(
                        "BLE parsable: {ble_mac} model=0x{:04X} payload={}B",
                        data.device_model,
                        data.raw_data.len()
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

/// Extracts the Apple (0x004C) entry out of a ManufacturerData property.
fn apple_manufacturer_data(changed: &HashMap<String, OwnedValue>) -> Option<Vec<u8>> {
    let mfg = changed.get("ManufacturerData")?;
    let map = <HashMap<u16, OwnedValue>>::try_from(mfg.clone()).ok()?;
    let apple = map.get(&APPLE_COMPANY_ID)?;
    <Vec<u8>>::try_from(apple.clone()).ok()
}

/// /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF -> AA:BB:CC:DD:EE:FF
fn mac_from_path(path: &str) -> Option<String> {
    let tail = path.rsplit('/').next()?;
    let hex = tail.strip_prefix("dev_")?;
    if hex.len() != 17 {
        return None;
    }
    Some(hex.replace('_', ":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mac_from_device_path() {
        assert_eq!(
            mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
    }

    #[test]
    fn rejects_non_device_paths() {
        assert_eq!(mac_from_path("/org/bluez/hci0"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_TOO_SHORT"), None);
        assert_eq!(mac_from_path(""), None);
    }
}
