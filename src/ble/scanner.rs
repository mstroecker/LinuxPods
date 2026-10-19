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
//! Discovery runs on every powered adapter, whatever it is called (#51). The
//! signals come from devices under any of them, as the subscription takes in
//! every signal bluetoothd sends.
//!
//! Accuracy note carried over from the Go docs: cleartext advertisements give
//! battery in 10% steps. Decrypting with a stored ENC_KEY yields 1% accuracy.

use std::collections::{HashMap, VecDeque};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream, proxy};

use super::parser::{ProximityData, parse_proximity_data};
use crate::bluez::{self, BLUEZ_SERVICE, Queue, drain};

/// The key of Apple's entry in a device's `ManufacturerData`.
pub const APPLE_COMPANY_ID: u16 = 0x004C;

const DEVICE_INTERFACE: &str = "org.bluez.Device1";

/// On every adapter's object.
#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter1 {
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
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

    /// Starts scanning on every powered adapter BlueZ has. One that fails is
    /// logged, and the others scan all the same. Returns the adapters now
    /// scanning, by path.
    ///
    /// Subscribe to [`Scanner::advertisements`] first: an address's first
    /// advertisement comes in the InterfacesAdded that announces its device,
    /// and one sent before the subscription is lost.
    ///
    /// Fails when BlueZ cannot be asked for its adapters, or scanning started
    /// on none of them.
    pub async fn start_discovery(&self) -> Result<Vec<String>> {
        let adapters = bluez::adapters(&self.conn)
            .await
            .context("failed to list Bluetooth adapters")?;
        anyhow::ensure!(!adapters.is_empty(), "BlueZ has no Bluetooth adapter");

        let mut scanning = Vec::new();
        for adapter in adapters {
            // bluetoothd refuses discovery on an adapter that is off.
            if !adapter.powered {
                tracing::info!("not scanning on {}: it is powered off", adapter.path);
                continue;
            }
            match self.start_discovery_on(&adapter.path).await {
                Ok(()) => {
                    tracing::info!("BLE discovery started on {}", adapter.path);
                    scanning.push(adapter.path);
                }
                Err(e) => {
                    tracing::warn!("failed to start BLE discovery on {}: {e:#}", adapter.path)
                }
            }
        }
        anyhow::ensure!(!scanning.is_empty(), "no Bluetooth adapter is scanning");
        Ok(scanning)
    }

    /// Sets an LE-only discovery filter on the adapter at `adapter`, and
    /// starts scanning there.
    pub async fn start_discovery_on(&self, adapter: &str) -> Result<()> {
        let adapter = Adapter1Proxy::new(&self.conn, adapter).await?;

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

    /// Yields advertisements as they arrive. Replaces the Go version's
    /// ScanForAirPods(timeout) polling call with a stream the coordinator drives.
    ///
    /// The stream may be read at the caller's pace, with calls to BlueZ on the
    /// scanner's connection in between - [`Scanner::start_discovery`] makes
    /// them after subscribing. A task of its own drains the subscription as
    /// the signals arrive, as [`crate::bluez::drain`] does: left in zbus's
    /// queue, 64 advertisements would stop zbus reading the connection, and
    /// the reply to such a call would never come. What waits for the reader
    /// is the latest advertisement from each address - see [`Latest`]. The
    /// task ends with the connection, or when the stream is dropped.
    pub async fn advertisements(
        &self,
    ) -> Result<impl futures_util::Stream<Item = Advertisement> + use<>> {
        // One rule, so one subscription: of two, the first's signals would go
        // unread while the bus confirmed the second, and 64 of them would stop
        // zbus reading the connection before the drain task began. It takes
        // in every signal bluetoothd sends - InterfacesAdded from its
        // ObjectManager at `/`, PropertiesChanged from each device - and one
        // stream keeps them in the order they were sent. zbus cannot check a
        // well-known sender, so whatever signal the connection gets comes
        // through: `device_properties` drops all but the two it reads by
        // their headers, before reading a body.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ_SERVICE)?
            .build();
        let signals = MessageStream::for_match_rule(rule, &self.conn, None)
            .await
            .context("failed to subscribe to BlueZ's signals")?;

        let adverts = signals.filter_map(|msg| async move {
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
        });
        Ok(drain::<Latest, _>(adverts))
    }
}

/// The advertisements waiting for the scanner's reader: the latest from each
/// address, in the order each address first came since it was last taken.
///
/// An advertiser sends as often as it likes, and a flood brings new addresses
/// as fast as it likes. Queued one per signal, they would pile up without
/// bound behind a reader that cannot keep up. An address's latest
/// advertisement says all its earlier ones did, as each carries the device's
/// whole state, so keeping only that one loses nothing. No address is dropped:
/// the one dropped could be a pair's.
#[derive(Debug, Default)]
struct Latest {
    /// Addresses with an advertisement waiting, oldest first.
    order: VecDeque<Address>,
    /// The latest advertisement from each address in `order`.
    adverts: HashMap<Address, Advertisement>,
}

impl Queue for Latest {
    type Item = Advertisement;

    fn push(&mut self, advert: Advertisement) {
        let address = advert.ble_mac;
        if self.adverts.insert(address, advert).is_none() {
            self.order.push_back(address);
        }
    }

    fn pop(&mut self) -> Option<Advertisement> {
        let address = self.order.pop_front()?;
        self.adverts.remove(&address)
    }
}

/// The path of the device a BlueZ signal is about, and the Device1 properties
/// it carries: all of them in the InterfacesAdded that announces a new
/// device, the changed ones in a PropertiesChanged. `None` for any other
/// signal.
///
/// InterfacesAdded comes from the ObjectManager and names the device in its
/// body; PropertiesChanged comes from the device itself. Either way the device
/// is under `/org/bluez`, where BlueZ keeps its adapters and their devices.
fn device_properties(msg: &Message) -> Option<(String, HashMap<String, OwnedValue>)> {
    let under_bluez = |path: &str| path.starts_with("/org/bluez/");
    let header = msg.header();
    match (header.interface()?.as_str(), header.member()?.as_str()) {
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path = header.path()?.to_string();
            if !under_bluez(&path) {
                return None;
            }
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
            if !under_bluez(path.as_str()) {
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

/// /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF -> AA:BB:CC:DD:EE:FF, under any
/// adapter.
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
        assert_eq!(
            mac_from_path("/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF"),
            Some(mac),
            "nor the adapter"
        );
    }

    /// An advertisement from `address`, told apart by `status`.
    fn advert(address: u8, status: u8) -> Advertisement {
        Advertisement {
            data: ProximityData {
                status,
                ..ProximityData::default()
            },
            ble_mac: Address::new([0x4A, 0, 0, 0, 0, address]),
        }
    }

    /// What `latest` gives out, as (address, status).
    fn take_all(latest: &mut Latest) -> Vec<(u8, u8)> {
        std::iter::from_fn(|| latest.pop())
            .map(|a| (a.ble_mac.0[5], a.data.status))
            .collect()
    }

    /// An address advertising over and over while the reader is busy is one
    /// advertisement waiting, the latest; once that is taken, the next is
    /// waiting again.
    #[test]
    fn an_address_has_its_latest_advertisement_waiting() {
        let mut latest = Latest::default();
        for status in 0..100 {
            latest.push(advert(1, status));
        }
        assert_eq!(take_all(&mut latest), [(1, 99)]);

        latest.push(advert(1, 100));
        assert_eq!(take_all(&mut latest), [(1, 100)]);
    }

    /// Every address keeps its own, in the order it first came: a flood of new
    /// addresses loses none of them.
    #[test]
    fn addresses_keep_their_advertisements_apart() {
        let mut latest = Latest::default();
        for address in 0..=255 {
            latest.push(advert(address, 0));
        }
        latest.push(advert(7, 1));
        let taken = take_all(&mut latest);
        assert_eq!(taken.len(), 256);
        assert_eq!(taken[..3], [(0, 0), (1, 0), (2, 0)]);
        assert_eq!(taken[7], (7, 1), "the latest, where the address first came");
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
        assert!(
            device_properties(&changed(
                "/org/example/dev_5C_4D_3F_B5_41_B6",
                DEVICE_INTERFACE,
                device1(&[0x07])
            ))
            .is_none(),
            "a change outside /org/bluez"
        );

        // The rest of what the subscription takes in goes by its headers.
        let acquired = Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameAcquired",
        )
        .unwrap()
        .build(&(":1.7",))
        .unwrap();
        assert!(device_properties(&acquired).is_none());
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
        assert_eq!(mac_from_path("/org/bluez/hci1"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_TOO_SHORT"), None);
        assert_eq!(mac_from_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_GG"), None);
        assert_eq!(mac_from_path(""), None);
    }
}
