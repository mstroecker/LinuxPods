//! The pipeline end to end (#73): from an advertisement or a connection on
//! D-Bus, or a packet on the AAP socket, through the real scanner, attachment,
//! coordinator and keystore, to the snapshot the UI renders and the battery
//! GNOME Settings shows.
//!
//! The coordinator's unit tests set its state up by hand and call one function
//! at a time. The per-device bugs of the past lived between those functions:
//! one device's key stored under another's MAC, a global pause blanking out
//! every other pair. Each test here runs the pieces together, against the fake
//! world of #72:
//!
//! - a `dbus-daemon` of its own ([`PrivateBus`]), never the real system or
//!   session bus
//! - a [`World`] on it: the fake BlueZ, with one adapter or several, and pairs
//!   of fake AirPods that advertise through it and answer AAP on a Unix socket
//!   each
//! - the app as `main.rs` wires it, less the UI and tray: the real [`Scanner`]
//!   feeding the real [`Coordinator`], with a [`Keystore`] in a temporary
//!   directory, and the real [`Attachment`] taking up the pairs BlueZ reports
//!   connected
//!
//! A test attaches a pair as a user would, by connecting it, and the app takes
//! it from there: an AAP link, and a battery for GNOME Settings, which the fake
//! BlueZ reports through [`World::batteries`].
//!
//! Time runs at real speed: each pod advertises once a second, from an
//! address of its own that the pair's IRK resolves and that rotates every
//! ten, and repeats its payload until something changes - so the app hears
//! a pod again when its state changes or its address does. Tests wait for
//! what they expect to see, under [`TIMEOUT`]. The 30-minute BLE cache expiry
//! stays with the coordinator's unit tests.
//!
//! Every key is generated at runtime, by the fake pairs; none is in the source.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bluer::Address;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zbus::Connection;

use linuxpods::aap::ear::ROLE_SWITCH_HOLD;
use linuxpods::aap::noise::{allow_off_packet, set_packet};
use linuxpods::aap::{NoiseMode, conversation, name};
use linuxpods::attachment::Attachment;
use linuxpods::ble::decrypt::random_key;
use linuxpods::ble::decrypt_for_device;
use linuxpods::ble::parser::{PodSide, parse_proximity_data};
use linuxpods::ble::rpa::{resolvable_address, resolves};
use linuxpods::ble::scanner::{Advertisement, Scanner};
use linuxpods::bluez::{BatteryProvider, ProviderConnection};
use linuxpods::harness::bluez::{VolumeAnswer, device_path};
use linuxpods::harness::pods::{ADDRESS_ROTATION, PRO_2_USB_C, PRO_3, Part, Pods};
use linuxpods::harness::{PrivateBus, World};
use linuxpods::keystore::{KEYS_FILE, Keystore, PairKeys};
use linuxpods::podstate::{Command, Coordinator, DataSource, Snapshot};
use linuxpods::volume::{self, Volume, VolumeControl};

/// The longest any one wait may take, so that a regression fails the test
/// instead of hanging CI. The slowest wait is for a pair's first BLE reading:
/// up to an advertising interval, as the scanner subscribes before it starts
/// discovery. The rest is headroom for a loaded runner.
const TIMEOUT: Duration = Duration::from_secs(15);

/// How often a wait on what nothing signals - the key file, the fake pair's
/// state - looks again.
const POLL: Duration = Duration::from_millis(20);

/// The real addresses of the fake pairs, from the range set aside for
/// documentation.
const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);
/// A third pair, which a test may have the fake BlueZ show without the AirPods
/// service: headphones, as far as the app can tell.
const HEADPHONES: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x03]);

/// Battery levels - left, right, case - none a multiple of ten. Advertised in
/// the clear they round down to their 10% step, so a BLE reading that shows
/// them was decrypted. They differ between the pairs, so a reading put down to
/// the wrong pair shows.
const PAIR_LEVELS: [u8; 3] = [43, 67, 51];
const OTHER_LEVELS: [u8; 3] = [36, 84, 22];

/// How many advertisers a flood brings.
const FLOOD: usize = 100;

/// The command byte of the key request, as the fake records what it is sent.
const KEY_REQUEST: u8 = 0x30;

/// How long the app gives a pair to answer the handshake, which a silent pair
/// never does.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an app started once the last one's discovery has ended may take
/// to hear a pair. The fake BlueZ took the pods' devices with that discovery,
/// as bluetoothd does, so each pod is announced afresh at its next
/// advertisement: up to an advertising interval after discovery starts
/// (#153). The rest is headroom for a loaded runner. While the fake kept the
/// devices, it took up to half a rotation, 5 s.
const HEARD_AFRESH: Duration = Duration::from_secs(4);

/// How long a test watches for what must not happen: a reconnect, an AAP
/// connection. Longer than the shortest waits between AAP attempts, 1 s and
/// then 2 s, so that a supervisor still running would show.
const QUIET: Duration = Duration::from_secs(4);

/// The fake BlueZ's adapters, by name. Most tests have the first alone, as
/// most machines do.
const HCI0: &str = "hci0";
const HCI1: &str = "hci1";

/// Logs to the test's output, which is shown when it fails. `RUST_LOG` picks
/// what; by default the app's info lines.
fn init_logs() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "linuxpods=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

/// A test's world: a bus of its own with the fake BlueZ and the pairs on it,
/// and the directories for the pairs' AAP sockets and the app's keystore.
struct Fixture {
    world: World,
    /// Where each pair listens for AAP. Short, as a Unix socket path longer
    /// than 108 bytes cannot be bound.
    sockets: TempDir,
    /// The app's data directory, where the keystore keeps `keys.json`.
    data: TempDir,
    bus: PrivateBus,
}

impl Fixture {
    /// A published world with one adapter, `hci0`, and `pairs` in it - each a
    /// real address, a product ID and its battery levels - paired with that
    /// adapter, in range and disconnected.
    async fn new(pairs: &[(Address, u16, [u8; 3])]) -> Self {
        let pairs: Vec<_> = pairs
            .iter()
            .map(|&(mac, product_id, levels)| (HCI0, mac, product_id, levels))
            .collect();
        Self::with_adapters(&[(HCI0, true)], &pairs).await
    }

    /// [`Fixture::new`], with `adapters` - each a name and whether it is
    /// powered - and each pair paired with the adapter named ahead of it.
    async fn with_adapters(
        adapters: &[(&str, bool)],
        pairs: &[(&str, Address, u16, [u8; 3])],
    ) -> Self {
        let bus = PrivateBus::start().expect("dbus-daemon starts");
        Self::on_bus(bus, adapters, pairs).await
    }

    /// [`Fixture::with_adapters`], on `bus`.
    async fn on_bus(
        bus: PrivateBus,
        adapters: &[(&str, bool)],
        pairs: &[(&str, Address, u16, [u8; 3])],
    ) -> Self {
        init_logs();
        let sockets = tempfile::Builder::new()
            .prefix("linuxpods-aap")
            .tempdir()
            .unwrap();
        let data = tempfile::tempdir().unwrap();
        let world = World::with_adapters(bus.address(), sockets.path(), adapters)
            .await
            .unwrap();
        let fixture = Self {
            world,
            sockets,
            data,
            bus,
        };
        for &(adapter, mac, product_id, levels) in pairs {
            fixture
                .world
                .add_pods_on(adapter, mac, product_id, "Fake AirPods")
                .await
                .unwrap();
            fixture.set_levels(mac, levels).await;
        }
        fixture.world.publish().await.unwrap();
        fixture
    }

    /// Sets a pair's battery levels: left, right and case.
    async fn set_levels(&self, mac: Address, [left, right, case]: [u8; 3]) {
        self.world
            .with_pods(mac, |pods| {
                pods.set_battery(Part::Left, Some(left));
                pods.set_battery(Part::Right, Some(right));
                pods.set_battery(Part::Case, Some(case));
            })
            .await
            .unwrap();
    }

    /// The key a pair encrypts its advertisements with, as its key packet
    /// carries it.
    async fn enc_key(&self, mac: Address) -> Vec<u8> {
        self.world.keys(mac).await.unwrap().enc_key.to_vec()
    }

    /// A pair's keys as its key packet carries them, and the app keeps them:
    /// the ENC_KEY and the IRK.
    async fn pair_keys(&self, mac: Address) -> PairKeys {
        let keys = self.world.keys(mac).await.unwrap();
        PairKeys::new(keys.enc_key, Some(keys.irk))
    }

    fn keystore(&self) -> Keystore {
        Keystore::with_dir(self.data.path().to_path_buf()).unwrap()
    }

    /// Saves the pairs' keys where the app looks for them, as though each had
    /// connected in an earlier session.
    async fn seed_keys(&self, macs: &[Address]) {
        let mut keys = HashMap::new();
        for &mac in macs {
            keys.insert(mac, self.pair_keys(mac).await);
        }
        self.keystore().save(&keys).unwrap();
    }

    /// Saves the pairs' ENC_KEYs alone, in format version 1, as a build from
    /// before the keystore kept IRKs left them.
    async fn seed_version_1_keys(&self, macs: &[Address]) {
        use base64::Engine as _;

        let mut keys = serde_json::Map::new();
        for &mac in macs {
            let key = base64::engine::general_purpose::STANDARD.encode(self.enc_key(mac).await);
            keys.insert(mac.to_string(), key.into());
        }
        let file = serde_json::json!({"version": 1, "keys": keys});
        std::fs::write(self.data.path().join(KEYS_FILE), file.to_string()).unwrap();
    }

    /// The key file's format version.
    fn stored_version(&self) -> u64 {
        let raw = std::fs::read(self.data.path().join(KEYS_FILE)).unwrap();
        let file: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        file["version"].as_u64().unwrap()
    }

    /// Waits until the key file holds `expected` and nothing else. The
    /// coordinator saves a key after it broadcasts it, and nothing tells when
    /// the save is done.
    async fn until_stored(&self, expected: &HashMap<Address, PairKeys>) {
        let keystore = &self.keystore();
        eventually("the keys to be saved", move || async move {
            keystore.load().ok().as_ref() == Some(expected)
        })
        .await;
    }

    /// Waits until GNOME Settings shows `expected` and nothing else: a level for
    /// each attached pair with a reading, by MAC. Read off the fake BlueZ, which
    /// tells no one when it changes.
    async fn until_batteries(&self, expected: &HashMap<Address, u8>) {
        let world = &self.world;
        eventually(
            &format!("GNOME Settings to show {expected:?}"),
            move || async move { world.batteries() == *expected },
        )
        .await;
    }

    /// Waits until the manager of the adapter called `adapter` holds
    /// `expected` and nothing else: the battery of each pair whose provider
    /// registered with that adapter's manager.
    async fn until_batteries_on(&self, adapter: &str, expected: &HashMap<Address, u8>) {
        let bluez = self.world.bluez();
        eventually(
            &format!("{adapter}'s manager to hold {expected:?}"),
            move || async move { bluez.batteries_on(adapter) == *expected },
        )
        .await;
    }

    /// Waits until some client - the app, as no other is on the test's bus -
    /// discovers on the adapter called `adapter`.
    async fn until_discovering(&self, adapter: &str) {
        let bluez = self.world.bluez();
        eventually(&format!("discovery on {adapter}"), move || async move {
            bluez.is_discovering(adapter)
        })
        .await;
    }

    /// Waits until nobody discovers on the adapter called `adapter` any more,
    /// and the fake BlueZ has taken along the devices it only heard
    /// advertising there, as bluetoothd does when the last discovery ends.
    async fn until_discovery_ended(&self, adapter: &str) {
        let bluez = self.world.bluez();
        eventually(
            &format!("discovery on {adapter} to end"),
            move || async move {
                !bluez.is_discovering(adapter) && bluez.advertisers_on(adapter).is_empty()
            },
        )
        .await;
    }

    /// Waits until a provider is registered with the adapter called
    /// `adapter`, and no more than one.
    async fn until_provider_on(&self, adapter: &str) {
        let bluez = self.world.bluez();
        eventually(
            &format!("a battery provider on {adapter}"),
            move || async move { bluez.providers_on(adapter) == 1 },
        )
        .await;
    }

    /// Waits until a pair is in `mode`, whichever way it got there.
    async fn until_mode(&self, mac: Address, mode: NoiseMode) {
        let world = &self.world;
        eventually(&format!("{mac} to switch to {mode}"), move || async move {
            world
                .with_pods(mac, |pods| pods.noise_mode())
                .await
                .unwrap()
                == mode
        })
        .await;
    }

    /// How many AAP connections the app has opened to a pair, those turned
    /// away included.
    async fn aap_attempts(&self, mac: Address) -> usize {
        self.world.aap_attempts(mac).await.unwrap()
    }

    /// Waits until the app has opened `count` AAP connections to a pair.
    async fn until_attempts(&self, mac: Address, count: usize) {
        eventually(
            &format!("AAP connection {count} to {mac}"),
            move || async move { self.aap_attempts(mac).await >= count },
        )
        .await;
    }

    /// Whether a pair holds an AAP connection from the app.
    async fn has_aap(&self, mac: Address) -> bool {
        self.world
            .with_pods(mac, |pods| pods.has_aap())
            .await
            .unwrap()
    }

    /// How many times the app has asked a pair for its keys, over all its
    /// connections.
    async fn key_requests(&self, mac: Address) -> usize {
        let received = self.world.received(mac).await.unwrap();
        received
            .iter()
            .filter(|packet| packet.get(4) == Some(&KEY_REQUEST))
            .count()
    }

    /// Waits until the app has asked a pair for its keys `count` times.
    async fn until_key_requests(&self, mac: Address, count: usize) {
        eventually(
            &format!("key request {count} to {mac}"),
            move || async move { self.key_requests(mac).await >= count },
        )
        .await;
    }

    /// Starts the app on this world - see [`App`].
    async fn start_app(&self) -> App {
        self.start_app_on(self.bus.connect_for_providers().await.unwrap())
            .await
    }

    /// [`Fixture::start_app`], with attachment on `bluez`, a connection to the
    /// test's bus that the test may have used first - one for providers, as
    /// attachment's is: see [`PrivateBus::connect_for_providers`].
    async fn start_app_on(&self, bluez: ProviderConnection) -> App {
        self.start_app_holding(bluez, ROLE_SWITCH_HOLD).await
    }

    /// [`Fixture::start_app`], its links holding an ear report for a role
    /// switch for `hold` at most - see `aap::ear::EarState`.
    async fn start_app_with_hold(&self, hold: Duration) -> App {
        self.start_app_holding(self.bus.connect_for_providers().await.unwrap(), hold)
            .await
    }

    /// [`Fixture::start_app_on`], holding as [`Fixture::start_app_with_hold`]
    /// does.
    async fn start_app_holding(&self, bluez: ProviderConnection, hold: Duration) -> App {
        let coordinator = Coordinator::with_aap_socket_dir_and_hold(
            Some(self.keystore()),
            self.sockets.path().to_path_buf(),
            hold,
        );
        let updates = coordinator.subscribe();

        // As main.rs's ble_task, on the test's bus: subscribed, then
        // discovering on every powered adapter, and following the adapters
        // from there. Back from `scan`, discovery is on wherever it can be.
        // Stopping the task stops it all.
        let conn = self.bus.connect().await.unwrap();
        let mut scanning = timeout(TIMEOUT, Scanner::with_connection(conn).scan())
            .await
            .expect("timed out starting the scanner")
            .expect("the scanner starts");
        let (handled, handled_rx) = mpsc::unbounded_channel();
        let feed = coordinator.clone();
        let listening = tokio::spawn(async move {
            while let Some(advert) = scanning.next().await {
                feed.handle_advertisement(advert.data.clone(), advert.ble_mac)
                    .await;
                let _ = handled.send(advert);
            }
        });

        // As main.rs's attachment task, on a connection of its own. Back from
        // `new`, the startup sweep is over: a pair the test connects from here
        // on is one attachment is told about, not one the sweep may find.
        let attachment = timeout(TIMEOUT, Attachment::new(bluez, coordinator.clone()))
            .await
            .expect("timed out starting attachment")
            .expect("attachment starts");
        let attaching = tokio::spawn(attachment.run());

        // As main.rs's volume task, on a connection of its own to the test's
        // bus, made once a speech level asks for it.
        let address = self.bus.address().to_string();
        let (lowering, volume) = Volume::new(
            &coordinator,
            Box::new(move || zbus::connection::Builder::address(address.as_str())),
        );
        let lowering = tokio::spawn(lowering.run());

        App {
            world: self.world.clone(),
            coordinator,
            updates,
            handled: handled_rx,
            listening,
            attaching,
            volume,
            lowering,
        }
    }
}

/// The app as `main.rs` wires it, less the UI and tray: the real scanner and
/// attachment on the test's bus, feeding the real coordinator, which keeps its
/// keys in the fixture's keystore and opens AAP to the pairs' sockets.
struct App {
    world: World,
    coordinator: Arc<Coordinator>,
    /// What the window, tray and attachment see.
    updates: watch::Receiver<Snapshot>,
    /// Every advertisement the coordinator has handled, once it has.
    handled: mpsc::UnboundedReceiver<Advertisement>,
    /// The scanner's task.
    listening: JoinHandle<()>,
    /// Attachment's task.
    attaching: JoinHandle<()>,
    /// Puts back the volumes lowered while the user spoke.
    volume: VolumeControl,
    /// The volume's task.
    lowering: JoinHandle<()>,
}

impl App {
    /// Connects a pair, and waits until the app has attached it: its AAP link
    /// is up. One connected already is the sweep's to attach; this only waits.
    async fn attach(&mut self, mac: Address) {
        self.world.connect(mac).await.unwrap();
        self.until(&format!("{mac} on AAP"), |s| s.is_connected(mac))
            .await;
    }

    /// Disconnects a pair, and waits until its AAP link is gone. Attachment may
    /// not have detached it by then: the pair hanging up AAP as it disconnects
    /// takes the link down as well.
    async fn detach(&mut self, mac: Address) {
        self.world.disconnect(mac).await.unwrap();
        self.until(&format!("{mac} off AAP"), |s| !s.is_connected(mac))
            .await;
    }

    /// Waits until the latest snapshot is `done`, and returns it.
    async fn until(&mut self, what: &str, done: impl FnMut(&Snapshot) -> bool) -> Snapshot {
        self.until_within(TIMEOUT, what, done).await
    }

    /// [`App::until`], waiting up to `budget` rather than [`TIMEOUT`].
    async fn until_within(
        &mut self,
        budget: Duration,
        what: &str,
        done: impl FnMut(&Snapshot) -> bool,
    ) -> Snapshot {
        // Cloned out: the borrow is a read lock every broadcast waits for.
        let found = match timeout(budget, self.updates.wait_for(done)).await {
            Ok(Ok(snapshot)) => Some(snapshot.clone()),
            Ok(Err(_)) => panic!("the coordinator went away, waiting for {what}"),
            Err(_) => None,
        };
        found.unwrap_or_else(|| {
            panic!(
                "timed out waiting for {what}; the last snapshot: {:#?}",
                *self.updates.borrow()
            )
        })
    }

    /// Waits until the coordinator has handled `count` more advertisements
    /// that `pick` picks out, skipping the rest.
    async fn handled(
        &mut self,
        what: &str,
        count: usize,
        pick: impl FnMut(&Advertisement) -> bool,
    ) {
        self.handled_within(TIMEOUT, what, count, pick).await;
    }

    /// [`App::handled`], waiting up to `budget` rather than [`TIMEOUT`].
    async fn handled_within(
        &mut self,
        budget: Duration,
        what: &str,
        count: usize,
        mut pick: impl FnMut(&Advertisement) -> bool,
    ) {
        let mut seen = 0;
        let waited = timeout(budget, async {
            while seen < count {
                let advert = self.handled.recv().await.expect("the scanner runs");
                if pick(&advert) {
                    seen += 1;
                }
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "timed out waiting for {what}: {seen} of {count}"
        );
    }

    /// Forgets the advertisements handled so far, for [`App::handled`] to
    /// count from here.
    fn skip_handled(&mut self) {
        while self.handled.try_recv().is_ok() {}
    }

    /// Stops the scanner, so that nothing more is heard over BLE: the pairs
    /// are out of earshot from here on.
    async fn stop_listening(&mut self) {
        self.listening.abort();
        let _ = (&mut self.listening).await;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.listening.abort();
        self.attaching.abort();
        self.lowering.abort();
    }
}

/// Waits until `done`, for what nothing signals: it looks again every
/// [`POLL`].
async fn eventually<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let waited = timeout(TIMEOUT, async {
        while !done().await {
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

/// A pair's battery levels - left, right, case - while its reading comes from
/// `source`.
fn levels(snapshot: &Snapshot, mac: Address, source: DataSource) -> Option<[u8; 3]> {
    let state = snapshot.states.get(&mac).filter(|s| s.source == source)?;
    Some([
        state.left_battery?,
        state.right_battery?,
        state.case_battery?,
    ])
}

/// A pair at `mac` with fresh keys and its batteries at `levels` - left,
/// right, case - that is none of the world's: only what the test hands on
/// of it is heard.
fn pods_at(mac: Address, [left, right, case]: [u8; 3]) -> Pods {
    let mut pods = Pods::new(PRO_3, mac);
    pods.set_battery(Part::Left, Some(left));
    pods.set_battery(Part::Right, Some(right));
    pods.set_battery(Part::Case, Some(case));
    pods
}

/// What GNOME Settings shows for a pair at `levels`: the lower earbud.
fn lowest([left, right, _]: [u8; 3]) -> u8 {
    left.min(right)
}

/// Whether `advert` came from the pair at `mac`: whether its key decrypts it.
/// The test's own check, apart from the coordinator's.
fn sent_by(advert: &Advertisement, mac: Address, key: &[u8]) -> bool {
    advert
        .data
        .encrypted_portion()
        .is_some_and(|encrypted| decrypt_for_device(encrypted, key, mac).is_ok())
}

/// A pair's key arrives over AAP when it connects and goes into the keystore,
/// and from then on its advertisements decrypt to the percent: in this
/// session, once the link is gone, and in the next one, which starts from the
/// keystore and needs no connection.
///
/// Before the key the pair's advertisements are dropped, as nothing could
/// tell them from a stranger's. That is checked once the coordinator has
/// handled one of them, so nothing has to be watched for a while.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_arrives_on_connect_is_stored_and_makes_the_ble_reading_exact() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let key = fixture.enc_key(PAIR).await;
    let mut app = fixture.start_app().await;

    app.handled("an advertisement from the pair", 1, |a| {
        sent_by(a, PAIR, &key)
    })
    .await;
    assert_eq!(
        app.coordinator.device_count().await,
        0,
        "kept without a key"
    );
    assert!(app.updates.borrow().known_keys.is_empty());

    app.attach(PAIR).await;
    app.until("the key and an AAP reading", |s| {
        s.known_keys == [PAIR] && levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    fixture
        .until_stored(&HashMap::from([(PAIR, fixture.pair_keys(PAIR).await)]))
        .await;

    app.detach(PAIR).await;
    app.until("an exact BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    // The next session, which loads the key from the keystore. The first
    // one's discovery ended with it, and took the pods' devices along, as
    // bluetoothd does (#153): the next app hears each pod afresh at its next
    // advertisement.
    drop(app);
    fixture.until_discovery_ended(HCI0).await;
    let mut app = fixture.start_app().await;
    let snapshot = app
        .until_within(
            HEARD_AFRESH,
            "an exact BLE reading in the next session",
            |s| levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS),
        )
        .await;
    assert_eq!(snapshot.known_keys, [PAIR]);
    assert!(snapshot.connected_macs.is_empty());
}

/// An app started once another app's discovery has ended hears both pods at
/// once, though neither payload has changed: bluetoothd removes the pods'
/// devices as the last discovery ends, so the next one announces each pod
/// afresh, in an `InterfacesAdded` with its payload, at its next
/// advertisement (#153). Kept, a device whose payload stays the same is
/// never signaled again, and a pod would be heard only from a new address:
/// the pods change address half a rotation apart, so within
/// [`HEARD_AFRESH`] one of them at most - whenever the test runs, and however
/// slowly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_started_after_discovery_ended_hears_the_pods_afresh() {
    assert!(HEARD_AFRESH < ADDRESS_ROTATION / 2, "one rotation at most");
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let key = fixture.enc_key(PAIR).await;
    let mut app = fixture.start_app().await;
    app.until("an exact BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    drop(app);
    fixture.until_discovery_ended(HCI0).await;
    let mut app = fixture.start_app().await;
    let mut heard = HashSet::new();
    app.handled_within(
        HEARD_AFRESH,
        "both pods, each from an address of its own",
        2,
        |a| sent_by(a, PAIR, &key) && heard.insert(a.ble_mac),
    )
    .await;
    app.until_within(HEARD_AFRESH, "the exact BLE reading again", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;
}

/// An AAP link to one pair leaves a second pair's BLE reading alone: the
/// second pair is still heard, and its reading follows it. A link supersedes
/// BLE for its own pair only - a global pause once blanked out every other
/// pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aap_to_one_pair_leaves_the_others_ble_reading_alone() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    fixture.seed_keys(&[PAIR, OTHER]).await;
    let mut app = fixture.start_app().await;
    app.until("both pairs' BLE readings", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Ble) == Some(OTHER_LEVELS)
    })
    .await;

    app.attach(PAIR).await;
    app.until("the first pair on AAP", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;

    // What only a fresh advertisement can bring.
    let changed = [OTHER_LEVELS[0] - 5, OTHER_LEVELS[1] - 3, OTHER_LEVELS[2]];
    fixture.set_levels(OTHER, changed).await;
    let snapshot = app
        .until("the other pair's BLE reading to follow it", |s| {
            levels(s, OTHER, DataSource::Ble) == Some(changed)
                && levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
        })
        .await;
    assert_eq!(snapshot.connected_macs, [PAIR]);
    let mut shown: Vec<_> = snapshot.states.keys().copied().collect();
    shown.sort_unstable();
    assert_eq!(shown, [PAIR, OTHER]);
}

/// Each pair's key goes into the keystore under its own MAC, and decrypts
/// that pair's advertisements. A read loop that outlived its link once read
/// the next link's socket under the old identity, and stored one device's
/// key under another's MAC. Covered: one pair connecting after another has
/// left, as when that happened, and two links at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_pairs_key_is_stored_under_its_own_mac() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let keys = HashMap::from([
        (PAIR, fixture.pair_keys(PAIR).await),
        (OTHER, fixture.pair_keys(OTHER).await),
    ]);
    assert_ne!(keys[&PAIR].enc_key, keys[&OTHER].enc_key);
    assert_ne!(keys[&PAIR].irk, keys[&OTHER].irk);
    let mut app = fixture.start_app().await;

    // One pair after the other.
    app.attach(PAIR).await;
    app.until("the first pair's key", |s| s.known_keys == [PAIR])
        .await;
    app.detach(PAIR).await;
    app.attach(OTHER).await;
    app.until("the second pair's key", |s| s.known_keys == [PAIR, OTHER])
        .await;
    fixture.until_stored(&keys).await;

    // Both at once, each link reading its own pair.
    app.attach(PAIR).await;
    app.until("both pairs on AAP", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Aap) == Some(OTHER_LEVELS)
    })
    .await;

    // Each key decrypts its own pair, and so each reading is the right one.
    app.detach(PAIR).await;
    app.detach(OTHER).await;
    app.until("both pairs' BLE readings", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Ble) == Some(OTHER_LEVELS)
    })
    .await;
    assert_eq!(fixture.keystore().load().unwrap(), keys);
}

/// When the link goes, the pair's reading falls back at once to its last
/// advertisement, cached while AAP superseded it: not when the pair next
/// advertises, which with the lid closed can be a long wait, and never to the
/// last AAP reading, which would pass for live.
///
/// The scanner stops before the disconnect, so nothing can be heard after it:
/// whatever BLE reading the snapshot shows is the cached one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_falls_back_to_the_cached_advertisement_at_once() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    // An AAP reading takes its connection state from the cached
    // advertisement, so once it has one, an advertisement is cached.
    app.until("an AAP reading over a cached advertisement", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
            && s.states[&PAIR].connection_state.is_some()
    })
    .await;

    app.stop_listening().await;
    let disconnected = Instant::now();
    app.detach(PAIR).await;

    let snapshot = app.until("the link to go", |s| !s.is_connected(PAIR)).await;
    let state = snapshot.states.get(&PAIR).expect("a reading at once");
    assert_eq!(
        levels(&snapshot, PAIR, DataSource::Ble),
        Some(PAIR_LEVELS),
        "{state:#?}"
    );
    assert!(
        state.last_seen.is_some_and(|seen| seen < disconnected),
        "the cached advertisement: {state:#?}"
    );
    assert_eq!(state.noise_mode, None, "BLE carries no mode");
}

/// Re-pairing gives a pair new keys, and its next connection replaces the
/// stored key: keys are requested on every connection, not only when none is
/// held. Until then the stale key fails validation without a word, and the
/// pair's advertisements are dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repaired_pairs_new_key_replaces_the_stored_one_on_the_next_connection() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let old = fixture.enc_key(PAIR).await;
    let mut app = fixture.start_app().await;
    app.until("a BLE reading with the stored key", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    // New levels with the new keys, at once: every advertisement under the
    // new key shows them, should one decrypt with the old.
    let changed = [29, 61, 47];
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.repair();
            pods.set_battery(Part::Left, Some(changed[0]));
            pods.set_battery(Part::Right, Some(changed[1]));
            pods.set_battery(Part::Case, Some(changed[2]));
        })
        .await
        .unwrap();
    let new = fixture.enc_key(PAIR).await;
    assert_ne!(new, old);

    app.handled("an advertisement under the new key", 1, |a| {
        sent_by(a, PAIR, &new)
    })
    .await;
    let snapshot = app.updates.borrow().clone();
    assert_eq!(
        levels(&snapshot, PAIR, DataSource::Ble),
        Some(PAIR_LEVELS),
        "the stale key decrypted the new advertisement"
    );

    app.attach(PAIR).await;
    fixture
        .until_stored(&HashMap::from([(PAIR, fixture.pair_keys(PAIR).await)]))
        .await;
    app.detach(PAIR).await;
    app.until("a BLE reading with the new key", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
}

/// A pair is identified by its address, which its IRK resolves, and then by
/// its key, and it stays identified as its address rotates: its reading
/// follows it to a new address. Every address it is heard from resolves with
/// its IRK (#134).
///
/// Both pods are out of the case, so the primary alone advertises, as real
/// pods do, and the pair is heard from one address at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_whose_address_resolves_is_identified_across_a_rotation() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.set_in_case(PodSide::Left, false);
            pods.set_in_case(PodSide::Right, false);
        })
        .await
        .unwrap();
    let keys = fixture.pair_keys(PAIR).await;
    let irk = keys.irk.unwrap();
    let mut app = fixture.start_app().await;
    app.until("an exact BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    // The address the pair was first heard from, then the next one: up to a
    // rotation and an advertising interval away, which is more than
    // `TIMEOUT` leaves room for on a loaded runner. The first is still among
    // those handled: a repeat of the same payload is never heard.
    let mut first = None;
    let next = "an advertisement from the pair's next address";
    app.handled_within(ADDRESS_ROTATION * 3, next, 1, |a| {
        if !sent_by(a, PAIR, &keys.enc_key) {
            return false;
        }
        assert!(resolves(&irk, a.ble_mac), "{} does not resolve", a.ble_mac);
        *first.get_or_insert(a.ble_mac) != a.ble_mac
    })
    .await;

    // What only an advertisement from here on can bring.
    let changed = [38, 72, 55];
    fixture.set_levels(PAIR, changed).await;
    let snapshot = app
        .until("the reading to follow the pair", |s| {
            levels(s, PAIR, DataSource::Ble) == Some(changed)
        })
        .await;
    let heard_from = snapshot.states[&PAIR].current_ble_mac.unwrap();
    assert_ne!(Some(heard_from), first, "the first address");
    assert!(resolves(&irk, heard_from));
}

/// A closed lid shows over BLE, and stays on show while the pods are quiet
/// in the closed case, as they go 4 s after it closes: the coordinator keeps
/// their last reading. Opened, the right pod comes back first, from a new
/// address, and the lid shows open (#71).
///
/// The first advertisement with the lid open after the pods went quiet is
/// one from after the lid opened: every one sent in the 4 s before had it
/// closed. Its address is one the right pod took coming back, not a
/// rotation: the pods as they were while quiet have it at no time from the
/// lid opening to its arrival.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_lid_stays_on_show_while_the_pods_are_quiet() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let keys = fixture.pair_keys(PAIR).await;
    let mut app = fixture.start_app().await;
    fn lid(snapshot: &Snapshot) -> Option<bool> {
        let state = snapshot.states.get(&PAIR)?;
        (state.source == DataSource::Ble).then_some(state.lid_open)?
    }
    app.until("the lid open", |s| lid(s) == Some(true)).await;
    let world = &fixture.world;
    let advertising = world.advertising(PAIR).await.unwrap();
    assert_eq!(advertising.len(), 2, "both pods, in the case");

    world
        .with_pods(PAIR, |pods| pods.set_lid(false))
        .await
        .unwrap();
    app.until("the lid closed", |s| lid(s) == Some(false)).await;
    eventually("the pods to go quiet", move || async move {
        world.advertising(PAIR).await.unwrap().is_empty()
    })
    .await;
    app.skip_handled();
    assert_eq!(lid(&app.updates.borrow()), Some(false), "the last kept");

    let quiet = world.with_pods(PAIR, |pods| pods.clone()).await.unwrap();
    let opened = world.now();
    world
        .with_pods(PAIR, |pods| pods.set_lid(true))
        .await
        .unwrap();
    let mut back = None;
    app.handled("a pod back, the lid open", 1, |a| {
        let open = sent_by(a, PAIR, &keys.enc_key) && a.data.lid_open == Some(true);
        if open {
            back = Some(a.clone());
        }
        open
    })
    .await;
    let arrived = world.now();
    let back = back.unwrap();
    assert_eq!(
        back.data.primary_pod(),
        PodSide::Right,
        "the right pod first"
    );
    assert!(resolves(&keys.irk.unwrap(), back.ble_mac));
    // Every rotation period in between, as each lasts longer than a second.
    let mut at = opened;
    loop {
        let rotated = quiet.address_at(PodSide::Right, at);
        assert_ne!(rotated, back.ble_mac, "a rotation, at {at:?}");
        if at >= arrived {
            break;
        }
        at = (at + Duration::from_secs(1)).min(arrived);
    }
    app.until("the lid open again", |s| lid(s) == Some(true))
        .await;
}

/// What the pair's key decrypts counts only from an address its IRK
/// resolves: from any other address - a replay, or a spoof - it is dropped,
/// though it decrypts. From the pair's own address the same advertisement
/// counts (#134).
///
/// The pair is none of the world's, so nothing advertises for it but the
/// test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_decrypts_from_an_address_that_does_not_resolve_is_dropped() {
    let fixture = Fixture::new(&[]).await;
    let pods = pods_at(PAIR, PAIR_LEVELS);
    let keys = pods.keys().clone();
    fixture
        .keystore()
        .save(&HashMap::from([(
            PAIR,
            PairKeys::new(keys.enc_key, Some(keys.irk)),
        )]))
        .unwrap();
    let mut app = fixture.start_app().await;

    let advert = pods.advertisement(PodSide::Right, Duration::ZERO);
    // A resolvable private address, as the pair's are, but another IRK's.
    let elsewhere = resolvable_address(&random_key(), [0x12, 0x34, 0x56]);
    assert!(!resolves(&keys.irk, elsewhere));
    let bluez = fixture.world.bluez();
    let heard = bluez.advertise(elsewhere, &advert.data).await.unwrap();
    assert!(heard, "advertised while the app discovers");
    app.handled("the advertisement from elsewhere", 1, |a| {
        a.ble_mac == elsewhere && sent_by(a, PAIR, &keys.enc_key)
    })
    .await;
    assert_eq!(app.coordinator.device_count().await, 0, "identified");

    assert!(resolves(&keys.irk, advert.address));
    bluez.advertise(advert.address, &advert.data).await.unwrap();
    app.until("the reading from the pair's own address", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;
}

/// A pair keyed before the keystore kept IRKs - a version 1 key file,
/// ENC_KEYs alone - is identified as it was then, by its key alone, until
/// its next connection brings its IRK. From then on its address has to
/// resolve too, and the key file is version 2 (#134).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_keyed_before_irks_were_kept_works_until_it_reconnects() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_version_1_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    app.until("an exact BLE reading, by the key alone", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;
    assert_eq!(fixture.stored_version(), 1, "nothing to write yet");

    app.attach(PAIR).await;
    let keys = fixture.pair_keys(PAIR).await;
    fixture
        .until_stored(&HashMap::from([(PAIR, keys.clone())]))
        .await;
    assert_eq!(fixture.stored_version(), 2);
    app.detach(PAIR).await;
    let changed = [38, 72, 55];
    fixture.set_levels(PAIR, changed).await;
    app.until("an exact BLE reading, now by the IRK and the key", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;

    // Off the air from here, so that only what the test hands the
    // coordinator is heard.
    app.stop_listening().await;
    let mut pods = pods_at(PAIR, [21, 32, 43]);
    pods.repair_with(fixture.world.keys(PAIR).await.unwrap());
    let advert = pods.advertisement(PodSide::Right, Duration::ZERO);
    let data = parse_proximity_data(&advert.data).unwrap();
    let elsewhere = resolvable_address(&random_key(), [0x12, 0x34, 0x56]);
    app.coordinator
        .handle_advertisement(data.clone(), elsewhere)
        .await;
    assert_eq!(
        levels(&app.updates.borrow(), PAIR, DataSource::Ble),
        Some(changed),
        "identified by the key alone"
    );
    app.coordinator
        .handle_advertisement(data, advert.address)
        .await;
    assert_eq!(
        levels(&app.updates.borrow(), PAIR, DataSource::Ble),
        Some([21, 32, 43]),
        "from the pair's own address"
    );
}

/// Off goes out with `0x34 01` right before it, as recent firmware takes Off
/// only once it is allowed; no other mode sends it, as it is a persistent
/// setting that syncs to the user's Apple devices. The fake pair refuses a
/// bare Off as the hardware does, so its switching to Off shows the order
/// held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_is_allowed_first_and_no_other_mode_is() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode from the startup dump", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancellation))
    })
    .await;

    let modes = [
        NoiseMode::Transparency,
        NoiseMode::Adaptive,
        NoiseMode::Off,
        NoiseMode::NoiseCancellation,
    ];
    for mode in modes {
        app.coordinator
            .command(PAIR, Command::NoiseControl(mode))
            .await
            .unwrap_or_else(|e| panic!("setting {mode} failed: {e:#}"));
        fixture.until_mode(PAIR, mode).await;
        app.until(&format!("{mode} on show"), |s| {
            s.states
                .get(&PAIR)
                .is_some_and(|state| state.noise_mode == Some(mode))
        })
        .await;
    }

    // The 0x09 settings family, which both commands belong to.
    let settings: Vec<Vec<u8>> = fixture
        .world
        .received(PAIR)
        .await
        .unwrap()
        .into_iter()
        .filter(|packet| packet.get(4) == Some(&0x09))
        .collect();
    let expected = [
        set_packet(NoiseMode::Transparency),
        set_packet(NoiseMode::Adaptive),
        allow_off_packet(true),
        set_packet(NoiseMode::Off),
        set_packet(NoiseMode::NoiseCancellation),
    ]
    .map(|packet| packet.to_vec());
    assert_eq!(settings, expected);
}

/// A pair that answers as AirPods 4 do - with Conversation Awareness on, as
/// theirs was, level 09 on every mode change, naming no mode, and an echo of
/// one change in four - has every mode set taken and kept on show: the level
/// changes no mode, and the echo, after the fourth change, confirms it. Each
/// change waits for the one before to be on show, so no echo can be a stale
/// report of a mode replaced meanwhile.
///
/// The echo is out once the pair has nothing due; a battery change made
/// then comes over the same connection after it, so once it shows, the
/// echo has been read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_answering_as_airpods_4_do_has_every_mode_confirmed() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.set_conversation_awareness(true);
            pods.quirks.idle_level_only = true;
            pods.quirks.echo_every = 4;
            pods.quirks.echo_delay = Duration::from_millis(500);
            pods.quirks.stale_report = None;
        })
        .await
        .unwrap();
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode from the startup dump", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancellation))
    })
    .await;

    let modes = [
        NoiseMode::Transparency,
        NoiseMode::Adaptive,
        NoiseMode::Off,
        NoiseMode::NoiseCancellation,
    ];
    for mode in modes {
        app.coordinator
            .command(PAIR, Command::NoiseControl(mode))
            .await
            .unwrap_or_else(|e| panic!("setting {mode} failed: {e:#}"));
        fixture.until_mode(PAIR, mode).await;
        app.until(&format!("{mode} on show"), |s| {
            s.states
                .get(&PAIR)
                .is_some_and(|state| state.noise_mode == Some(mode))
        })
        .await;
    }

    let world = &fixture.world;
    eventually("the echo of the fourth change", move || async move {
        world
            .with_pods(PAIR, |pods| pods.next_due().is_none())
            .await
            .unwrap()
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(LATER)))
        .await
        .unwrap();
    let snapshot = app
        .until("the battery change", |s| {
            s.states
                .get(&PAIR)
                .is_some_and(|state| state.left_battery == Some(LATER))
        })
        .await;
    assert_eq!(
        snapshot.states[&PAIR].noise_mode,
        Some(NoiseMode::NoiseCancellation)
    );
    assert_eq!(snapshot.states[&PAIR].speech_level, Some(0x09));
}

/// After Off, a report of the mode it replaced does not bring that mode back:
/// in the #71 capture the pair reported Noise Cancellation 0.33 s after Off
/// was set from it, and Off itself only at 1.4 s, and the window showed Noise
/// Cancellation for the second in between (#113). The fake pair does the same.
///
/// Every snapshot from the command on is watched, until a battery change made
/// once the echo is out has arrived: it comes over the same connection, after
/// the stale report and the echo, so by then both have been read. The stale
/// report comes well inside the 2 s the app holds it off for, and the pair
/// reports Noise Cancellation for a whole second before the echo: a snapshot
/// showing it would be seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_report_after_off_does_not_bring_the_old_mode_back() {
    /// A left pod level the pair has not had yet, none of the startup dump's.
    const LATER: u8 = 29;

    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode from the startup dump", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancellation))
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.quirks.stale_report = Some(Duration::from_millis(330));
            pods.quirks.echo_every = 1;
            pods.quirks.echo_delay = Duration::from_millis(1400);
        })
        .await
        .unwrap();

    // Every mode on show for the pair, each change once, until the battery
    // change. Cloned out of the borrow, which every broadcast waits for.
    fn read(updates: &mut watch::Receiver<Snapshot>) -> (Option<NoiseMode>, Option<u8>) {
        let snapshot = updates.borrow_and_update();
        let state = snapshot.states.get(&PAIR);
        (
            state.and_then(|s| s.noise_mode),
            state.and_then(|s| s.left_battery),
        )
    }
    let mut updates = app.coordinator.subscribe();
    // Read here, not in the task, which may first run after the command.
    let mut shown = vec![read(&mut updates).0];
    let watching = tokio::spawn(async move {
        while updates.changed().await.is_ok() {
            let (mode, left) = read(&mut updates);
            if shown.last() != Some(&mode) {
                shown.push(mode);
            }
            if left == Some(LATER) {
                break;
            }
        }
        shown
    });

    app.coordinator
        .command(PAIR, Command::NoiseControl(NoiseMode::Off))
        .await
        .unwrap();
    // The pair has the command, and with it the stale report and the echo
    // queued; then both are out.
    fixture.until_mode(PAIR, NoiseMode::Off).await;
    let world = &fixture.world;
    eventually("the echo of Off", move || async move {
        world
            .with_pods(PAIR, |pods| pods.next_due().is_none())
            .await
            .unwrap()
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(LATER)))
        .await
        .unwrap();

    let shown = timeout(TIMEOUT, watching)
        .await
        .expect("timed out waiting for the battery change")
        .unwrap();
    assert_eq!(
        shown,
        [Some(NoiseMode::NoiseCancellation), Some(NoiseMode::Off)],
        "Off, and nothing after it"
    );
}

/// The pair's Conversation Awareness, as the snapshot shows it.
fn conversation_awareness(snapshot: &Snapshot, mac: Address) -> Option<bool> {
    snapshot
        .states
        .get(&mac)
        .and_then(|s| s.conversation_awareness)
}

/// Conversation Awareness comes from the startup dump, and a switch goes out
/// as the 0x28 setting, the pair taking it and the snapshot showing it at
/// once. Nothing reports it back, so the next connection's dump is what
/// shows the pair kept it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_awareness_is_read_from_the_dump_and_switched() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the setting from the startup dump: off", |s| {
        conversation_awareness(s, PAIR) == Some(false)
    })
    .await;

    app.coordinator
        .command(PAIR, Command::ConversationAwareness(true))
        .await
        .unwrap();
    assert_eq!(
        conversation_awareness(&app.updates.borrow(), PAIR),
        Some(true)
    );
    let world = &fixture.world;
    eventually("the pair to take the setting", move || async move {
        world
            .with_pods(PAIR, |pods| pods.conversation_awareness())
            .await
            .unwrap()
    })
    .await;
    let received = fixture.world.received(PAIR).await.unwrap();
    assert!(received.contains(&conversation::set_packet(true).to_vec()));

    app.detach(PAIR).await;
    let off_aap = conversation_awareness(&app.updates.borrow(), PAIR);
    assert_eq!(off_aap, None, "the setting goes with the link");
    app.attach(PAIR).await;
    app.until("the setting from the next dump: on", |s| {
        conversation_awareness(s, PAIR) == Some(true)
    })
    .await;
}

/// The pods' own name, as the snapshot shows it.
fn pods_name(snapshot: &Snapshot, mac: Address) -> Option<String> {
    snapshot.states.get(&mac).and_then(|s| s.name.clone())
}

/// The rename packets the app has sent a pair, over all its connections.
async fn renames_sent(fixture: &Fixture, mac: Address) -> Vec<Vec<u8>> {
    let received = fixture.world.received(mac).await.unwrap();
    received
        .into_iter()
        .filter(|packet| packet.get(4) == Some(&name::RENAME_OPCODE))
        .collect()
}

/// The name comes from the startup dump, and a rename goes out as 0x1A, the
/// snapshot showing it at once, as nothing answers it. The pods take it, the
/// next connection's dump reports it, and BlueZ's `Name`, and so its alias,
/// follow once the pair has connected again (#160). The name the app shows
/// for the pair - the switcher's - is the new one at once, before BlueZ's.
/// An alias set in BlueZ is left alone, and stays on show in the switcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_is_recorded_and_the_next_connection_reports_it() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    let snapshot = app
        .until("the name from the startup dump", |s| {
            pods_name(s, PAIR).as_deref() == Some("Fake AirPods")
        })
        .await;
    assert_eq!(snapshot.device_name(PAIR), Some("Fake AirPods"));
    assert!(!snapshot.has_alias(PAIR));

    app.coordinator
        .command(PAIR, Command::Rename("Test Pods".into()))
        .await
        .unwrap();
    let snapshot = app.updates.borrow().clone();
    assert_eq!(pods_name(&snapshot, PAIR).as_deref(), Some("Test Pods"));
    assert_eq!(snapshot.device_name(PAIR), Some("Fake AirPods"));
    assert_eq!(snapshot.display_name(PAIR).as_deref(), Some("Test Pods"));
    let world = &fixture.world;
    eventually("the pair to take the name", move || async move {
        world.name(PAIR).await.unwrap() == b"Test Pods"
    })
    .await;
    assert_eq!(
        renames_sent(&fixture, PAIR).await,
        [name::rename_packet("Test Pods").unwrap()]
    );

    app.detach(PAIR).await;
    let snapshot = app.updates.borrow().clone();
    assert_eq!(
        pods_name(&snapshot, PAIR),
        None,
        "the name goes with the link"
    );
    assert_eq!(snapshot.device_name(PAIR), Some("Fake AirPods"));
    app.attach(PAIR).await;
    let snapshot = app
        .until("the new name, from the dump and from BlueZ", |s| {
            pods_name(s, PAIR).as_deref() == Some("Test Pods")
                && s.device_name(PAIR) == Some("Test Pods")
        })
        .await;
    assert!(!snapshot.has_alias(PAIR));

    // An alias set in GNOME Settings: the switcher keeps it, whatever the
    // pods are called.
    app.detach(PAIR).await;
    fixture
        .world
        .set_alias(PAIR, Some("Kitchen"))
        .await
        .unwrap();
    app.attach(PAIR).await;
    let snapshot = app
        .until("the alias, read as the pair connects", |s| {
            s.device_name(PAIR) == Some("Kitchen")
        })
        .await;
    assert!(snapshot.has_alias(PAIR));
    let snapshot = app
        .until("the pods' own name", |s| {
            pods_name(s, PAIR).as_deref() == Some("Test Pods")
        })
        .await;
    assert_eq!(snapshot.display_name(PAIR).as_deref(), Some("Kitchen"));
}

/// A name the pods would not take - empty, with a NUL, over 127 bytes, in
/// bytes rather than characters - is refused, and never sent; nor is any
/// rename without a link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_the_pods_would_not_take_is_never_sent() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    let unlinked = app
        .coordinator
        .command(PAIR, Command::Rename("Test Pods".into()))
        .await;
    assert!(
        format!("{:#}", unlinked.unwrap_err()).contains("no AAP connection"),
        "no link, no rename"
    );

    app.attach(PAIR).await;
    app.until("the name from the startup dump", |s| {
        pods_name(s, PAIR).is_some()
    })
    .await;
    // 43 characters, 129 bytes.
    for refused in [
        String::new(),
        "a\0b".into(),
        "a".repeat(128),
        "’".repeat(43),
    ] {
        let failed = app
            .coordinator
            .command(PAIR, Command::Rename(refused.clone()))
            .await;
        assert!(failed.is_err(), "{refused:?} refused");
    }
    assert_eq!(
        pods_name(&app.updates.borrow(), PAIR).as_deref(),
        Some("Fake AirPods")
    );

    // Sent after them, and the first to arrive.
    let allowed = "’".repeat(42);
    app.coordinator
        .command(PAIR, Command::Rename(allowed.clone()))
        .await
        .unwrap();
    let world = &fixture.world;
    let expected = allowed.clone();
    eventually("the pair to take the allowed name", move || {
        let expected = expected.clone();
        async move { world.name(PAIR).await.unwrap() == expected.as_bytes() }
    })
    .await;
    assert_eq!(
        renames_sent(&fixture, PAIR).await,
        [name::rename_packet(&allowed).unwrap()]
    );
}

/// What a snapshot shows of the pair while the user speaks: the noise mode,
/// the speech level and the left pod's battery.
type Speaking = (Option<NoiseMode>, Option<u8>, Option<u8>);

fn speaking(updates: &mut watch::Receiver<Snapshot>) -> Speaking {
    let snapshot = updates.borrow_and_update();
    let state = snapshot.states.get(&PAIR);
    (
        state.and_then(|s| s.noise_mode),
        state.and_then(|s| s.speech_level),
        state.and_then(|s| s.left_battery),
    )
}

/// The modes and the levels a test saw, each change once.
type Seen = (Vec<Option<NoiseMode>>, Vec<Option<u8>>);

/// Watches every snapshot from now on until the left pod is at `last`, and
/// returns the modes and the levels shown, each change once. The first of
/// each is read here, before anything is sent: a task first run after the
/// change it waits for would miss the state before it.
fn watch_speech(mut updates: watch::Receiver<Snapshot>, last: u8) -> JoinHandle<Seen> {
    let (mode, level, _) = speaking(&mut updates);
    let (mut modes, mut levels) = (vec![mode], vec![level]);
    tokio::spawn(async move {
        while updates.changed().await.is_ok() {
            let (mode, level, left) = speaking(&mut updates);
            if modes.last() != Some(&mode) {
                modes.push(mode);
            }
            if levels.last() != Some(&level) {
                levels.push(level);
            }
            if left == Some(last) {
                return (modes, levels);
            }
        }
        panic!("the coordinator went away");
    })
}

/// Waits until the pair has nothing more due - every level and report of a
/// stretch of speech, every echo, out - then changes the left pod to `last`,
/// which comes over the same connection after all of them.
async fn after_everything_due(fixture: &Fixture, last: u8) {
    let world = &fixture.world;
    eventually("every level and report", move || async move {
        world
            .with_pods(PAIR, |pods| pods.next_due().is_none())
            .await
            .unwrap()
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(last)))
        .await
        .unwrap();
}

/// A pair on AAP with Conversation Awareness on, its mode and the level the
/// dump ends with on show.
async fn aware_on_aap() -> (Fixture, App) {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_conversation_awareness(true))
        .await
        .unwrap();
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode, the setting and level 09 from the dump", |s| {
        s.states.get(&PAIR).is_some_and(|state| {
            state.noise_mode == Some(NoiseMode::NoiseCancellation)
                && state.conversation_awareness == Some(true)
                && state.speech_level == Some(0x09)
        })
    })
    .await;
    (fixture, app)
}

/// A stretch of speech as the #150 run had it - levels 01 and 02, the pods
/// switching to Transparency and back, 03 to 08 and 09 - arrives, and the
/// mode on show follows the pods there and back: Transparency is what they
/// are in. 3 s, as the run's second stretch.
///
/// Every snapshot from the speech on is watched, until a battery change made
/// once the pair has nothing more due has arrived. A level is a target, and
/// the window skips any it is too slow for, so the levels are checked as
/// what can be seen in order, of which the ones held for long - speaking,
/// for 3 s - must be.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stretch_of_speech_arrives_and_the_mode_comes_back() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let (fixture, app) = aware_on_aap().await;
    let watching = watch_speech(app.coordinator.subscribe(), LATER);
    fixture
        .world
        .speak(PAIR, Duration::from_secs(3))
        .await
        .unwrap();
    after_everything_due(&fixture, LATER).await;

    let (modes, levels) = timeout(TIMEOUT, watching)
        .await
        .expect("timed out waiting for the battery change")
        .unwrap();
    use NoiseMode::{NoiseCancellation, Transparency};
    assert_eq!(
        modes,
        [
            Some(NoiseCancellation),
            Some(Transparency),
            Some(NoiseCancellation)
        ]
    );
    let stretch = [0x09, 0x01, 0x02, 0x03, 0x07, 0x08, 0x09].map(Some);
    let mut rest = stretch.iter();
    assert!(
        levels.iter().all(|level| rest.any(|l| l == level)),
        "{levels:02x?}: not in the order of {stretch:02x?}"
    );
    assert!(
        levels.contains(&Some(0x01)) || levels.contains(&Some(0x02)),
        "{levels:02x?}: no speaking level"
    );
    assert_eq!(levels.first(), Some(&Some(0x09)));
    assert_eq!(levels.last(), Some(&Some(0x09)));
}

/// A mode set while the pods are in Transparency for speech stays on show:
/// the fake pair goes back to it after the speech, so nothing reports
/// Transparency or the mode before the speech again, and the echo and the
/// levels framing the change, 08 and 09, bring no mode back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mode_set_during_speech_stays_on_show() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let (fixture, mut app) = aware_on_aap().await;
    fixture
        .world
        .speak(PAIR, Duration::from_secs(3))
        .await
        .unwrap();
    app.until("Transparency for the speech", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::Transparency))
    })
    .await;

    let watching = watch_speech(app.coordinator.subscribe(), LATER);
    app.coordinator
        .command(PAIR, Command::NoiseControl(NoiseMode::Adaptive))
        .await
        .unwrap();
    fixture.until_mode(PAIR, NoiseMode::Adaptive).await;
    after_everything_due(&fixture, LATER).await;

    let (modes, levels) = timeout(TIMEOUT, watching)
        .await
        .expect("timed out waiting for the battery change")
        .unwrap();
    assert_eq!(
        modes,
        [Some(NoiseMode::Transparency), Some(NoiseMode::Adaptive)]
    );
    assert_eq!(levels.last(), Some(&Some(0x09)));
}

/// The volume a pair plays at in the volume tests, which speech lowers to a
/// fifth: 20.
const VOLUME: u16 = 100;

/// How the pair answers a volume the app sets in the volume tests: late, and
/// rounded down to even, as bluetoothd has real pods answer - the property
/// changes only once they do, to the volume they report. How real pods
/// answer is untried (#150); this keeps the app from counting on the set
/// reading back at once, or as set.
const SLOW_ANSWER: VolumeAnswer = VolumeAnswer {
    delay: Duration::from_millis(150),
    to_even: true,
};

/// The volumes the app set on the pair's transports, in order.
fn volumes_set(fixture: &Fixture) -> Vec<u16> {
    let sets = fixture.world.volume_sets(PAIR);
    sets.into_iter().map(|(_, volume)| volume).collect()
}

/// Waits until the volumes the app set on the pair's transports are `done`.
/// Read off the fake BlueZ, which tells no one when it changes.
async fn until_volumes(fixture: &Fixture, what: &str, done: impl Fn(&[u16]) -> bool + Copy) {
    eventually(what, move || async move { done(&volumes_set(fixture)) }).await;
}

/// Waits until the pair's transport reads `volume`.
async fn until_volume(fixture: &Fixture, what: &str, volume: u16) {
    let world = &fixture.world;
    eventually(what, move || async move {
        world.volume(PAIR).await == Some(volume)
    })
    .await;
}

/// [`aware_on_aap`], with audio playing to the pair at [`VOLUME`], which
/// answers the app's volumes as [`SLOW_ANSWER`] says.
async fn aware_and_playing() -> (Fixture, App) {
    let (fixture, app) = aware_on_aap().await;
    fixture.world.set_volume_answer(PAIR, SLOW_ANSWER).unwrap();
    fixture.world.set_playing(PAIR, true).await.unwrap();
    fixture.world.set_volume(PAIR, VOLUME).await.unwrap();
    (fixture, app)
}

/// [`aware_and_playing`], with the pair speaking for `length`, and lowered:
/// the pair has answered the lowering.
async fn lowered_by_speech(length: Duration) -> (Fixture, App) {
    let (fixture, app) = aware_and_playing().await;
    fixture.world.speak(PAIR, length).await.unwrap();
    until_volume(&fixture, "the volume lowered", VOLUME / 5).await;
    (fixture, app)
}

/// Waits until the app has handled every level of a stretch of speech: the
/// pair has nothing more due, and a battery change made after that, which
/// comes over the same connection, has reached the snapshot. The volume
/// hears of the levels from the coordinator before that change.
async fn after_the_speech(fixture: &Fixture, app: &mut App) {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;
    after_everything_due(fixture, LATER).await;
    app.until("the battery change after the speech", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.left_battery == Some(LATER))
    })
    .await;
}

/// Speech lowers the pair's own transport at once to a fifth of its volume,
/// and once it is over the volume comes back to exactly what it was, in
/// steps rather than at once (#150) - with the pair answering each step late
/// and rounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_lowers_the_volume_and_it_fades_back() {
    let (fixture, mut app) = aware_and_playing().await;
    fixture
        .world
        .speak(PAIR, Duration::from_secs(3))
        .await
        .unwrap();
    after_the_speech(&fixture, &mut app).await;

    until_volumes(&fixture, "the volume lowered, and back", |set| {
        set.last() == Some(&VOLUME)
    })
    .await;
    let volumes = volumes_set(&fixture);
    assert_eq!(volumes[0], VOLUME / 5, "down at once: {volumes:?}");
    assert!(
        volumes.len() >= 3 && volumes.windows(2).all(|w| w[0] < w[1]),
        "up in steps: {volumes:?}"
    );
    until_volume(&fixture, "the volume back", VOLUME).await;
    assert!(app.volume.settled().await);
}

/// A volume the user set while it was lowered is theirs: the end of the
/// speech brings no fade, and nothing is put back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_volume_the_user_set_while_lowered_is_kept() {
    let (fixture, mut app) = lowered_by_speech(Duration::from_secs(3)).await;
    fixture.world.set_volume(PAIR, 50).await.unwrap();

    after_the_speech(&fixture, &mut app).await;
    assert!(app.volume.settled().await, "nothing more to set");
    assert_eq!(volumes_set(&fixture), [VOLUME / 5]);
    assert_eq!(fixture.world.volume(PAIR).await, Some(50));
}

/// The link going in the middle of the speech puts the volume back at once:
/// no level would ever bring it back otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_going_puts_the_volume_back() {
    let (fixture, mut app) = lowered_by_speech(Duration::from_secs(10)).await;

    fixture.world.hang_up(PAIR).await.unwrap();
    app.until("the link gone", |s| !s.is_connected(PAIR)).await;
    until_volume(&fixture, "the volume put back", VOLUME).await;
    assert_eq!(volumes_set(&fixture), [VOLUME / 5, VOLUME], "in one step");
}

/// The pair disconnecting in the middle of the speech takes its transport
/// along, and bluetoothd keeps the lowered volume for the next one. Once
/// the pair is back, linked and playing at that volume, it is put back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_volume_lowered_when_the_pair_went_is_put_back_when_it_returns() {
    let (fixture, mut app) = lowered_by_speech(Duration::from_secs(10)).await;

    app.detach(PAIR).await;
    assert_eq!(fixture.world.volume(PAIR).await, None, "no transport");
    assert!(!app.volume.settled().await, "waiting to put it back");
    app.attach(PAIR).await;
    fixture.world.set_playing(PAIR, true).await.unwrap();
    until_volume(&fixture, "the volume put back", VOLUME).await;
    // In one step when the look for the transport finds it; faded, when the
    // level 09 of the next link's dump came with the transport there.
    let volumes = volumes_set(&fixture);
    assert_eq!(volumes.first(), Some(&(VOLUME / 5)), "{volumes:?}");
    assert_eq!(volumes.last(), Some(&VOLUME), "{volumes:?}");
    assert!(volumes.windows(2).all(|w| w[0] < w[1]), "{volumes:?}");
    assert!(app.volume.settled().await);
}

/// The pair back at another volume than was lowered to: that is the user's,
/// or another host's, and is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_back_at_another_volume_is_left_alone() {
    let (fixture, mut app) = lowered_by_speech(Duration::from_secs(10)).await;

    app.detach(PAIR).await;
    fixture.world.set_volume(PAIR, 50).await.unwrap();
    app.attach(PAIR).await;
    fixture.world.set_playing(PAIR, true).await.unwrap();
    let volume = &app.volume;
    eventually("the volume left as it is", move || async move {
        volume.settled().await
    })
    .await;
    assert_eq!(volumes_set(&fixture), [VOLUME / 5]);
    assert_eq!(fixture.world.volume(PAIR).await, Some(50));
}

/// With nothing playing to the pair - no A2DP transport at all - speech has
/// nothing to lower, and the app sets nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_with_nothing_playing_lowers_nothing() {
    let (fixture, mut app) = aware_on_aap().await;
    fixture
        .world
        .speak(PAIR, Duration::from_secs(3))
        .await
        .unwrap();
    after_the_speech(&fixture, &mut app).await;
    assert!(app.volume.settled().await, "nothing to set");
    assert_eq!(volumes_set(&fixture), [] as [u16; 0]);
    assert_eq!(fixture.world.volume(PAIR).await, None);
}

/// In a call speech is the call: nothing is lowered, and A2DP comes back
/// after the call at the volume it had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_in_a_call_lowers_nothing() {
    let (fixture, mut app) = aware_and_playing().await;
    fixture.world.set_call(PAIR, true).await.unwrap();
    fixture
        .world
        .speak(PAIR, Duration::from_secs(3))
        .await
        .unwrap();
    after_the_speech(&fixture, &mut app).await;
    assert!(app.volume.settled().await, "nothing to set");
    assert_eq!(volumes_set(&fixture), [] as [u16; 0]);

    fixture.world.set_call(PAIR, false).await.unwrap();
    assert_eq!(fixture.world.volume(PAIR).await, Some(VOLUME));
}

/// The app quitting in the middle of the speech puts the volume back, as
/// `main.rs` asks before the runtime goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_puts_the_volume_back() {
    let (fixture, app) = lowered_by_speech(Duration::from_secs(10)).await;

    timeout(volume::RESTORE_LIMIT, app.volume.restore_all())
        .await
        .expect("put back in time");
    assert_eq!(volumes_set(&fixture), [VOLUME / 5, VOLUME]);
    until_volume(&fixture, "the volume put back", VOLUME).await;
}

/// Whether each pod is in an ear, left then right, while the pair's reading
/// comes over AAP.
fn in_ear(snapshot: &Snapshot, mac: Address) -> Option<(Option<bool>, Option<bool>)> {
    let state = snapshot
        .states
        .get(&mac)
        .filter(|s| s.source == DataSource::Aap)?;
    Some((state.left_in_ear, state.right_in_ear))
}

/// Whether each pod is in an ear, left then right, wherever the pair's
/// reading comes from.
fn ears_shown(snapshot: &Snapshot, mac: Address) -> InEar {
    let state = snapshot.states.get(&mac)?;
    Some((state.left_in_ear, state.right_in_ear))
}

/// In-ear as [`in_ear`] and [`ears_shown`] give it.
type InEar = Option<(Option<bool>, Option<bool>)>;

/// A pair with both pods worn, the left one primary, on AAP, and nothing
/// heard over BLE: the scanner stops before the pair connects, so no
/// advertisement is ever cached, and whatever in-ear the snapshot shows is
/// the link's. Its links hold an ear report for a role switch for `hold` at
/// most.
async fn worn_on_aap(hold: Duration) -> (Fixture, App) {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.set_in_ear(PodSide::Left, true);
            pods.set_in_ear(PodSide::Right, true);
        })
        .await
        .unwrap();
    let mut app = fixture.start_app_with_hold(hold).await;
    app.stop_listening().await;
    app.attach(PAIR).await;
    let snapshot = app
        .until("both pods worn, as the link reports it", |s| {
            in_ear(s, PAIR) == Some((Some(true), Some(true)))
        })
        .await;
    assert_eq!(
        snapshot.states[&PAIR].connection_state, None,
        "no advertisement cached"
    );
    (fixture, app)
}

/// A pod taken out over AAP shows at once, with no advertisement to tell
/// it: a connected pair's payload changes seldom, and the icons followed its
/// advertisements alone, seconds late or not at all (#142). Back in, it
/// shows again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_taken_out_shows_from_the_link_alone() {
    let (fixture, mut app) = worn_on_aap(ROLE_SWITCH_HOLD).await;
    for (in_ear_now, what) in [(false, "out"), (true, "back in")] {
        fixture
            .world
            .with_pods(PAIR, |pods| pods.set_in_ear(PodSide::Right, in_ear_now))
            .await
            .unwrap();
        let snapshot = app
            .until(&format!("the right pod {what}"), |s| {
                in_ear(s, PAIR) == Some((Some(true), Some(in_ear_now)))
            })
            .await;
        assert_eq!(snapshot.states[&PAIR].connection_state, None);
    }
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_in_case(PodSide::Left, true))
        .await
        .unwrap();
    app.until("the left pod in the case", |s| {
        in_ear(s, PAIR) == Some((Some(false), Some(true)))
    })
    .await;
}

/// The primary pod taken out moves the role to the other, as the #142 and
/// #71 captures have it: the ear report in the new order comes first, and the
/// fake's primary pod packet 10 ms later. Left and right stay where they are
/// throughout.
///
/// Every snapshot from the left pod going out on is watched, until a battery
/// change made once the switch is over has arrived: it comes over the same
/// connection, after the ear report and the primary pod packets, so by then
/// all of them have been read. The switch is made only once the watcher has
/// seen the left pod go out.
///
/// The app holds the ear report for the primary pod packet for [`TIMEOUT`]
/// here, not the 250 ms of [`ROLE_SWITCH_HOLD`]: on a loaded machine the
/// fake's packet 10 ms later could come later than that, and the report be
/// taken as it is - the pods shown swapped, as a swap by hand is. So what is
/// checked is that the report waits for the primary pod packet, and is read
/// with it; that the hold ends on its own is the next test's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_switch_keeps_left_and_right() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let (fixture, mut app) = worn_on_aap(TIMEOUT).await;

    // Every in-ear on show, each change once, until the battery change.
    // Cloned out of the borrow, which every broadcast waits for.
    fn read(updates: &mut watch::Receiver<Snapshot>) -> (InEar, Option<u8>) {
        let snapshot = updates.borrow_and_update();
        let left = snapshot.states.get(&PAIR).and_then(|s| s.left_battery);
        (in_ear(&snapshot, PAIR), left)
    }
    let left_out = Some((Some(false), Some(true)));
    let mut updates = app.coordinator.subscribe();
    let mut shown = vec![read(&mut updates).0];
    let (watching_tx, watching) = tokio::sync::oneshot::channel();
    let watcher = tokio::spawn(async move {
        let mut watching_tx = Some(watching_tx);
        while updates.changed().await.is_ok() {
            let (in_ear, left) = read(&mut updates);
            if shown.last() != Some(&in_ear) {
                shown.push(in_ear);
            }
            if in_ear == left_out
                && let Some(tx) = watching_tx.take()
            {
                let _ = tx.send(());
            }
            if left == Some(LATER) {
                break;
            }
        }
        shown
    });

    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_in_ear(PodSide::Left, false))
        .await
        .unwrap();
    timeout(TIMEOUT, watching)
        .await
        .expect("timed out waiting for the left pod to go out")
        .unwrap();

    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_primary(PodSide::Right))
        .await
        .unwrap();
    let world = &fixture.world;
    eventually("the primary pod packets", move || async move {
        world
            .with_pods(PAIR, |pods| pods.next_due().is_none())
            .await
            .unwrap()
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(LATER)))
        .await
        .unwrap();

    let shown = timeout(TIMEOUT, watcher)
        .await
        .expect("timed out waiting for the battery change")
        .unwrap();
    assert_eq!(
        shown,
        [Some((Some(true), Some(true))), left_out],
        "the left pod out, and nothing after it"
    );

    // The right pod, primary now, out as well; then the left one back in.
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_in_ear(PodSide::Right, false))
        .await
        .unwrap();
    app.until("both pods out", |s| {
        in_ear(s, PAIR) == Some((Some(false), Some(false)))
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_in_ear(PodSide::Left, true))
        .await
        .unwrap();
    app.until("the left pod back in", |s| {
        in_ear(s, PAIR) == Some((Some(true), Some(false)))
    })
    .await;
}

/// Both pods swapped by hand in one ear report, with no role switch after
/// it: the report is held for a primary pod packet that never comes, and
/// taken once the hold is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swap_with_no_role_switch_shows_once_the_hold_is_over() {
    let (fixture, mut app) = worn_on_aap(ROLE_SWITCH_HOLD).await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_in_ear(PodSide::Left, false))
        .await
        .unwrap();
    app.until("the left pod out", |s| {
        in_ear(s, PAIR) == Some((Some(false), Some(true)))
    })
    .await;

    let swapped = Instant::now();
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.set_in_ear(PodSide::Left, true);
            pods.set_in_ear(PodSide::Right, false);
        })
        .await
        .unwrap();
    app.until("the right pod out, the left one in", |s| {
        in_ear(s, PAIR) == Some((Some(true), Some(false)))
    })
    .await;
    assert!(
        swapped.elapsed() >= ROLE_SWITCH_HOLD,
        "taken without a hold"
    );
}

/// The link tells where the pods are only after the battery, as a Pro 3
/// does: the primary pod (0x08), then 0x0C, 0x0E and 0x2E, then the ear
/// report (0x06) - and the mode later still, in the startup dump after a
/// second 0x0C (capture 4 of #71). Until the link has given both the primary
/// and the ear report, in-ear comes from the cached advertisement, so nothing
/// in between shows a pod in an ear that is not: read with the left pod taken
/// for primary, the report would put the left pod in an ear and the right one
/// out.
///
/// The advertisement cached has both pods in the case. Then, unheard, the
/// right pod becomes primary and goes in an ear, and the left one comes out.
/// Every in-ear on show is watched from before the pair connects until a
/// battery change made once the link has told the places: it comes over the
/// same connection, after them. The startup dump is held back meanwhile, so
/// the mode is still unknown then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_tells_the_ears_after_the_battery_and_the_mode_after_both() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    let in_case = Some((Some(false), Some(false)));
    app.until("a BLE reading, both pods in the case", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS) && ears_shown(s, PAIR) == in_case
    })
    .await;
    app.stop_listening().await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.set_primary(PodSide::Right);
            pods.set_in_ear(PodSide::Right, true);
            pods.set_in_case(PodSide::Left, false);
            // Longer than the test: the mode stays unknown.
            pods.quirks.dump_delay = Duration::from_secs(3600);
        })
        .await
        .unwrap();

    // Every in-ear on show, each change once, until the battery change, and
    // the mode then. Cloned out of the borrow, which every broadcast waits
    // for.
    type Shown = (InEar, Option<u8>, Option<NoiseMode>);
    fn read(updates: &mut watch::Receiver<Snapshot>) -> Shown {
        let snapshot = updates.borrow_and_update();
        let state = snapshot.states.get(&PAIR);
        (
            ears_shown(&snapshot, PAIR),
            state.and_then(|s| s.left_battery),
            state.and_then(|s| s.noise_mode),
        )
    }
    let mut updates = app.coordinator.subscribe();
    // Read here, not in the task, which may first run after the connect.
    let mut shown = vec![read(&mut updates).0];
    let watching = tokio::spawn(async move {
        while updates.changed().await.is_ok() {
            let (in_ear, left, mode) = read(&mut updates);
            if shown.last() != Some(&in_ear) {
                shown.push(in_ear);
            }
            if left == Some(LATER) {
                return (shown, mode);
            }
        }
        panic!("the coordinator went away");
    });

    app.attach(PAIR).await;
    let right_in = Some((Some(false), Some(true)));
    app.until("the right pod in an ear, as the link reports it", |s| {
        in_ear(s, PAIR) == right_in
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(LATER)))
        .await
        .unwrap();

    let (shown, mode) = timeout(TIMEOUT, watching)
        .await
        .expect("timed out waiting for the battery change")
        .unwrap();
    assert_eq!(
        shown,
        [in_case, right_in],
        "the cached in-ear, then the link's"
    );
    assert_eq!(mode, None, "a mode ahead of the startup dump");
}

/// Two quick commands to a pair go out in the order they were issued, however
/// their results are waited for, and the mode on show is the one sent last
/// (#54). Here each is spawned apart, as two clicks were, and the second's
/// result waited for first. Each used to send, then record its mode, in a task
/// of its own: the two went out in either order, and the mode recorded last
/// could be the one sent first, while the pair was in the other. Off goes
/// first, two packets to the other's one, so that it would be the one
/// overtaken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_commands_go_out_in_the_order_issued_and_the_last_stays_on_show() {
    two_quick_switches(true).await;
}

/// [`quick_commands_go_out_in_the_order_issued_and_the_last_stays_on_show`],
/// with neither spawned: both issued, then the second's result awaited
/// before the first's. Issued when first polled, as the commands were, the
/// second went out whole before the first had started, every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_goes_out_in_its_place_though_awaited_later() {
    two_quick_switches(false).await;
}

/// Quick changes of mode on a pair answering as a Pro 3 does - an echo of
/// every change 1 s later, and after an Off a report of the mode it replaced
/// at 0.33 s - end with the mode last set on show. Noise Cancellation to Off
/// to Transparency, 150 ms apart: Off's stale report and its echo come after
/// Transparency has replaced Off, and may briefly replay Noise Cancellation
/// and Off, which is accepted (see `aap::noise::ModeState`). Transparency's
/// echo comes last and settles it.
///
/// Once the pair has nothing more due, a battery change comes over the same
/// connection after every report, so once it shows, all of them have been
/// read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_changes_end_with_the_mode_last_set() {
    /// A left pod level the pair has not had yet.
    const LATER: u8 = 29;

    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode from the startup dump", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancellation))
    })
    .await;

    for mode in [NoiseMode::Off, NoiseMode::Transparency] {
        app.coordinator
            .command(PAIR, Command::NoiseControl(mode))
            .await
            .unwrap_or_else(|e| panic!("setting {mode} failed: {e:#}"));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let world = &fixture.world;
    eventually("every echo and report", move || async move {
        world
            .with_pods(PAIR, |pods| pods.next_due().is_none())
            .await
            .unwrap()
    })
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.set_battery(Part::Left, Some(LATER)))
        .await
        .unwrap();
    let snapshot = app
        .until("the battery change", |s| {
            s.states
                .get(&PAIR)
                .is_some_and(|state| state.left_battery == Some(LATER))
        })
        .await;
    assert_eq!(
        snapshot.states[&PAIR].noise_mode,
        Some(NoiseMode::Transparency)
    );
}

/// Issues Off then Transparency to a fresh pair, spawned apart or not, waits
/// for Transparency's result first, and checks the pair got allow-Off, Off
/// and Transparency in that order, and the snapshot shows Transparency.
///
/// The pair reports no mode here - no echo, and no report of the mode Off
/// replaced - so that nothing but the commands sets it.
async fn two_quick_switches(spawned: bool) {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.quirks.echo_every = 0;
            // Off's report of the mode it replaced, taken once Transparency
            // has replaced Off, may briefly put Noise Cancellation back on
            // show, which is accepted (see `aap::noise::ModeState`) - but this
            // test reads the snapshot right after the commands.
            pods.quirks.stale_report = None;
        })
        .await
        .unwrap();
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the mode from the startup dump", |s| {
        s.states
            .get(&PAIR)
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancellation))
    })
    .await;

    let first = app
        .coordinator
        .command(PAIR, Command::NoiseControl(NoiseMode::Off));
    let second = app
        .coordinator
        .command(PAIR, Command::NoiseControl(NoiseMode::Transparency));
    type Done = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
    let (first, second): (Done, Done) = if spawned {
        let (first, second) = (tokio::spawn(first), tokio::spawn(second));
        (
            Box::pin(async { first.await.unwrap() }),
            Box::pin(async { second.await.unwrap() }),
        )
    } else {
        (Box::pin(first), Box::pin(second))
    };
    for (mode, done) in [(NoiseMode::Transparency, second), (NoiseMode::Off, first)] {
        timeout(TIMEOUT, done)
            .await
            .expect("timed out waiting for a command")
            .unwrap_or_else(|e| panic!("switching to {mode} failed: {e:#}"));
    }

    // Each recorded its mode before it was done, so the snapshot has the
    // last one already.
    let shown = app.updates.borrow().states[&PAIR].noise_mode;
    assert_eq!(shown, Some(NoiseMode::Transparency));
    fixture.until_mode(PAIR, NoiseMode::Transparency).await;
    let settings: Vec<Vec<u8>> = fixture
        .world
        .received(PAIR)
        .await
        .unwrap()
        .into_iter()
        .filter(|packet| packet.get(4) == Some(&0x09))
        .collect();
    let expected = [
        allow_off_packet(true).to_vec(),
        set_packet(NoiseMode::Off).to_vec(),
        set_packet(NoiseMode::Transparency).to_vec(),
    ];
    assert_eq!(settings, expected);
}

/// A command's caller hears how it went: a switch for a pair on AAP goes
/// out, and one for a pair gone off AAP fails, rather than being dropped
/// without a word (#54).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_to_a_pair_off_aap_fails_to_its_caller() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;

    let transparency = Command::NoiseControl(NoiseMode::Transparency);
    timeout(TIMEOUT, app.coordinator.command(PAIR, transparency))
        .await
        .expect("timed out waiting for the switch")
        .unwrap();
    fixture.until_mode(PAIR, NoiseMode::Transparency).await;

    app.detach(PAIR).await;
    let adaptive = Command::NoiseControl(NoiseMode::Adaptive);
    let failed = timeout(TIMEOUT, app.coordinator.command(PAIR, adaptive))
        .await
        .expect("timed out waiting for the switch")
        .unwrap_err();
    assert_eq!(
        format!("{failed:#}"),
        format!("switch to Adaptive not sent: no AAP connection to {PAIR}")
    );
    let sent = fixture.world.received(PAIR).await.unwrap();
    assert!(
        !sent.contains(&set_packet(NoiseMode::Adaptive).to_vec()),
        "sent after all"
    );
}

/// A flood of advertisements no stored key decrypts, as BLE spam brings, is
/// dropped on arrival: no entry and no broadcast. Kept, each would be an entry
/// under an address that never comes back, and a snapshot for every consumer:
/// what a flood fed on.
///
/// The pair is on AAP, settled: nothing in a snapshot changes from here on,
/// so neither its link nor its own advertisements broadcast, and a broadcast
/// can only come from the flood - none from before is still on its way, as
/// the coordinator sends each before the next change. Nothing has to be
/// watched for a while: the check comes after the coordinator has handled
/// every advertiser, each known by its address, and it broadcasts within the
/// handling or not at all.
///
/// Both pods are in the open case, so both advertise, each from an address
/// of its own and each calling itself primary. Neither's advertisement
/// changes what a snapshot shows: it shows no primary pod (#152).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_of_undecryptable_advertisements_is_neither_stored_nor_broadcast() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let key = fixture.enc_key(PAIR).await;
    // Connected before the app hears it, so that every advertisement it sends
    // says so, and none changes what a snapshot takes from the last.
    fixture.world.connect(PAIR).await.unwrap();
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the pair settled on AAP", |s| {
        s.known_keys == [PAIR]
            && s.states.get(&PAIR).is_some_and(|state| {
                state.source == DataSource::Aap
                    && state.noise_mode.is_some()
                    && state.identity.is_some()
                    && state.connection_state.is_some()
            })
    })
    .await;
    app.updates.mark_unchanged();

    app.skip_handled();
    fixture.world.flood(FLOOD as u32).await.unwrap();
    // By address, so that the count is of advertisers each heard from an
    // address of its own.
    let mut advertisers = std::collections::HashSet::new();
    app.handled("every advertiser in the flood", FLOOD, |a| {
        !sent_by(a, PAIR, &key) && advertisers.insert(a.ble_mac)
    })
    .await;

    assert!(
        !app.updates.has_changed().unwrap(),
        "broadcast: {:#?}",
        *app.updates.borrow()
    );
    assert_eq!(app.coordinator.device_count().await, 1, "stored");
    let snapshot = app.updates.borrow().clone();
    assert_eq!(snapshot.states.keys().collect::<Vec<_>>(), [&PAIR]);
    assert_eq!(snapshot.known_keys, [PAIR]);
    assert_eq!(
        fixture.keystore().load().unwrap(),
        HashMap::from([(PAIR, fixture.pair_keys(PAIR).await)])
    );
}

/// An address that advertises once reaches the coordinator. bluetoothd
/// announces a new address with InterfacesAdded, its first advertisement
/// among the properties, and only the ones after as PropertiesChanged.
/// Regression (#86): the scanner read only the second, and lost the first
/// advertisement from every address - here, the only one.
///
/// The pair is none of the world's, so nothing advertises for it but the
/// test, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_address_that_advertises_once_reaches_the_coordinator() {
    let fixture = Fixture::new(&[]).await;
    let pods = pods_at(PAIR, PAIR_LEVELS);
    let keys = pods.keys();
    fixture
        .keystore()
        .save(&HashMap::from([(
            PAIR,
            PairKeys::new(keys.enc_key, Some(keys.irk)),
        )]))
        .unwrap();
    let mut app = fixture.start_app().await;

    let advert = pods.advertisement(PodSide::Right, Duration::ZERO);
    let bluez = fixture.world.bluez();
    let heard = bluez.advertise(advert.address, &advert.data).await.unwrap();
    assert!(heard, "advertised while the app discovers");
    app.until("the reading it brings", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;
}

/// A pair connected when the app starts is attached by the startup sweep, as
/// BlueZ signals nothing for it: its AAP link comes up, and once a reading
/// arrives, GNOME Settings shows its lower earbud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_connected_before_startup_is_attached_by_the_sweep() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.world.connect(PAIR).await.unwrap();
    let mut app = fixture.start_app().await;

    app.until("an AAP reading", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
}

/// A pair connecting while the app runs is attached, and disconnecting
/// detaches it: its link goes, and its battery is withdrawn - for being
/// detached, not for want of a reading, as the cached advertisement shows in
/// the link's place at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_connecting_later_is_attached_and_detached_when_it_disconnects() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    app.until("a BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    app.attach(PAIR).await;
    app.until("an AAP reading", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;

    app.detach(PAIR).await;
    fixture.until_batteries(&HashMap::new()).await;
    let snapshot = app.updates.borrow().clone();
    assert_eq!(
        levels(&snapshot, PAIR, DataSource::Ble),
        Some(PAIR_LEVELS),
        "a reading, though no battery"
    );
}

/// Only AirPods are attached, as their service tells (#40): a paired device
/// that does not list it gets no AAP link, connected when the app starts or
/// connecting later, and so no battery either. Its alias says AirPods, which
/// plays no part. Behind it is a fake pair that answers AAP, so attaching it
/// would show, as a link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_that_isnt_airpods_is_never_attached() {
    let fixture =
        Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS), (HEADPHONES, PRO_3, PAIR_LEVELS)]).await;
    let bluez = fixture.world.bluez();
    bluez.set_airpods_service(HEADPHONES, false).await.unwrap();
    fixture.world.connect(HEADPHONES).await.unwrap();
    let mut app = fixture.start_app().await;
    assert!(
        app.updates.borrow().connected_macs.is_empty(),
        "attached by the sweep"
    );

    fixture.world.disconnect(HEADPHONES).await.unwrap();
    fixture.world.connect(HEADPHONES).await.unwrap();
    // Connected changes are handled one at a time, in the order they came:
    // once the pair is attached, the headphones' connection has been handled.
    app.attach(PAIR).await;
    assert_eq!(
        app.updates.borrow().connected_macs,
        [PAIR],
        "attached as they connected"
    );
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
}

/// A pair being paired for the first time connects before service discovery
/// lists the AirPods service, so it is not recognized as it connects. It is
/// attached when its services change to list it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_is_attached_once_its_services_mark_it_as_airpods() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let bluez = fixture.world.bluez();
    bluez.set_airpods_service(PAIR, false).await.unwrap();
    let mut app = fixture.start_app().await;

    fixture.world.connect(PAIR).await.unwrap();
    // Connected changes are handled one at a time, in the order they came:
    // once the other pair is attached, the first one's connection has been
    // handled.
    app.attach(OTHER).await;
    assert!(
        !app.updates.borrow().is_connected(PAIR),
        "attached before it could be recognized"
    );

    bluez.set_airpods_service(PAIR, true).await.unwrap();
    app.until("the pair attached", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
}

/// Two pairs are attached and detached each on its own: the sweep attaches
/// both, each battery follows its own pair's reading, and one disconnecting
/// takes its own link and battery along, and leaves the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_pairs_attach_and_detach_independently() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    fixture.world.connect(PAIR).await.unwrap();
    fixture.world.connect(OTHER).await.unwrap();
    let mut app = fixture.start_app().await;
    app.until("both pairs on AAP", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Aap) == Some(OTHER_LEVELS)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([
            (PAIR, lowest(PAIR_LEVELS)),
            (OTHER, lowest(OTHER_LEVELS)),
        ]))
        .await;

    let changed = [OTHER_LEVELS[0] - 5, OTHER_LEVELS[1], OTHER_LEVELS[2]];
    fixture.set_levels(OTHER, changed).await;
    fixture
        .until_batteries(&HashMap::from([
            (PAIR, lowest(PAIR_LEVELS)),
            (OTHER, lowest(changed)),
        ]))
        .await;

    app.detach(PAIR).await;
    fixture
        .until_batteries(&HashMap::from([(OTHER, lowest(changed))]))
        .await;
    assert_eq!(app.updates.borrow().connected_macs, [OTHER]);
}

/// A pair is attached though BlueZ refuses the battery provider on its
/// adapter, as an adapter does with no BatteryProviderManager1 - a BlueZ
/// without one. It gets AAP, if no GNOME Settings battery, and a pair on an
/// adapter that took its provider gets its battery all the same. Regression:
/// a refused provider stopped attachment altogether, and every pair stayed on
/// BLE, a pair new to the app with no key to be read by.
///
/// Refused here as the first adapter has no battery API at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_battery_provider_refused_on_one_adapter_leaves_attachment_working() {
    let fixture = Fixture::with_adapters(
        &[(HCI0, true), (HCI1, true)],
        &[
            (HCI0, PAIR, PRO_3, PAIR_LEVELS),
            (HCI1, OTHER, PRO_2_USB_C, OTHER_LEVELS),
        ],
    )
    .await;
    fixture.world.bluez().drop_battery_api(HCI0).await.unwrap();
    let mut app = fixture.start_app().await;

    app.attach(PAIR).await;
    app.attach(OTHER).await;
    app.until("both keys and AAP readings", |s| {
        s.known_keys == [PAIR, OTHER]
            && levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Aap) == Some(OTHER_LEVELS)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([(OTHER, lowest(OTHER_LEVELS))]))
        .await;
}

/// A provider BlueZ holds already for the app's connection is registered:
/// BlueZ refuses it again as `AlreadyExists`, and that refusal is the
/// registration standing. Its battery shows. Regression (rules review of
/// #121): the refusal left the adapter with no provider until its next
/// change, so registering again after a change that changed nothing for
/// BlueZ - a restart seen twice - lost GNOME Settings its battery.
///
/// Registered here by the test, on the connection the app then runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_battery_provider_registered_already_is_registered() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let bluez = fixture.bus.connect_for_providers().await.unwrap();
    let _registered = BatteryProvider::register(&bluez, "/org/bluez/hci0")
        .await
        .unwrap();
    let mut app = fixture.start_app_on(bluez).await;

    app.attach(PAIR).await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
    assert_eq!(fixture.world.bluez().providers_on(HCI0), 1);
}

/// A machine whose only adapter is not `hci0` - a USB dongle, an internal
/// adapter switched off, a re-enumeration after resume (#51). Its pair is
/// heard over BLE, attaches over AAP, and its battery goes to that adapter's
/// manager. Regression: `/org/bluez/hci0` was hardcoded, so discovery and the
/// battery provider failed at startup, and nothing was heard over BLE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_on_an_adapter_other_than_hci0_is_heard_attached_and_given_its_battery() {
    let fixture =
        Fixture::with_adapters(&[(HCI1, true)], &[(HCI1, PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    app.until("a BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    app.attach(PAIR).await;
    app.until("an AAP reading", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    fixture
        .until_batteries_on(HCI1, &HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
}

/// Two adapters with a pair on each. Both adapters hear both pairs, so each
/// advertisement is signaled twice, from a device of its address under each:
/// the two make one reading per pair, to the percent. Each pair attaches
/// through its own paired device, and its battery goes to its own adapter's
/// manager - never through the device the other adapter made of its address,
/// and no manager is offered a battery for a device not its own, which
/// bluetoothd would ignore with a warning. Regression: only `hci0` was
/// scanned and had a provider, so a pair on another adapter got no battery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairs_on_two_adapters_are_heard_and_each_battery_goes_to_its_own_adapter() {
    let fixture = Fixture::with_adapters(
        &[(HCI0, true), (HCI1, true)],
        &[
            (HCI0, PAIR, PRO_3, PAIR_LEVELS),
            (HCI1, OTHER, PRO_2_USB_C, OTHER_LEVELS),
        ],
    )
    .await;
    fixture.seed_keys(&[PAIR, OTHER]).await;
    let mut app = fixture.start_app().await;
    app.until("both pairs' BLE readings", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Ble) == Some(OTHER_LEVELS)
    })
    .await;
    let bluez = fixture.world.bluez();
    let (first, second) = (bluez.advertisers_on(HCI0), bluez.advertisers_on(HCI1));
    assert!(
        !first.is_disjoint(&second),
        "no address heard by both: {first:?}, {second:?}"
    );
    let mut shown: Vec<_> = app.updates.borrow().states.keys().copied().collect();
    shown.sort_unstable();
    assert_eq!(shown, [PAIR, OTHER], "one reading each");

    app.attach(PAIR).await;
    app.attach(OTHER).await;
    let snapshot = app
        .until("both pairs' AAP readings", |s| {
            levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
                && levels(s, OTHER, DataSource::Aap) == Some(OTHER_LEVELS)
        })
        .await;
    let mut linked = snapshot.connected_macs.clone();
    linked.sort_unstable();
    assert_eq!(
        linked,
        [PAIR, OTHER],
        "a link to an address the pairs advertise from"
    );
    fixture
        .until_batteries_on(HCI0, &HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
    fixture
        .until_batteries_on(HCI1, &HashMap::from([(OTHER, lowest(OTHER_LEVELS))]))
        .await;
    assert_eq!(
        fixture.world.batteries().len(),
        2,
        "a battery on another device"
    );
    let ignored = bluez.ignored_batteries();
    assert!(ignored.is_empty(), "offered the wrong manager: {ignored:?}");
}

/// An adapter that is off is not scanned, as bluetoothd would refuse, and
/// the one that is on scans all the same. It hears both pairs, the one paired
/// with the adapter that is off as well. Regression: `hci0` was the one
/// adapter scanned, and discovery failed there for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adapter_powered_off_is_not_scanned_and_the_other_is() {
    let fixture = Fixture::with_adapters(
        &[(HCI0, false), (HCI1, true)],
        &[
            (HCI0, OTHER, PRO_2_USB_C, OTHER_LEVELS),
            (HCI1, PAIR, PRO_3, PAIR_LEVELS),
        ],
    )
    .await;
    fixture.seed_keys(&[PAIR, OTHER]).await;
    let mut app = fixture.start_app().await;
    let bluez = fixture.world.bluez();
    assert!(
        !bluez.is_discovering(HCI0),
        "discovering on an adapter that is off"
    );
    assert!(bluez.is_discovering(HCI1));

    app.until("both pairs, through the adapter that is on", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
            && levels(s, OTHER, DataSource::Ble) == Some(OTHER_LEVELS)
    })
    .await;
    assert!(
        bluez.advertisers_on(HCI0).is_empty(),
        "heard by the adapter that is off"
    );
}

/// Advertisements piling up while attachment is busy - a flood, or a crowd,
/// while a pair's AAP link comes up - never stall it. Regression: they filled
/// the connection watch's queue, zbus stopped reading the connection until
/// there was room, and the reply to attachment's next call to BlueZ never
/// came. No pair was attached or detached again.
///
/// All of it while attachment takes up the first pair - its calls to BlueZ
/// and its AAP link coming up - and attachment handles one connection at a
/// time, so the second pair's connection is handled after it, with one flood
/// queued ahead of it and another behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_while_a_pair_attaches_does_not_stall_attachment() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let mut app = fixture.start_app().await;

    // Repeated from one address, every advertisement after an advertiser's
    // first is a `PropertiesChanged` - of `RSSI` alone, as the payload is the
    // same, as bluetoothd signals a repeat - the one signal the connection
    // watch subscribes to: ten advertisers bring it 100 before the second pair's
    // connection and 200 after, each more than zbus's 64-message queue holds.
    // Few addresses, so that few signals are `InterfacesAdded`, which only the
    // scanner reads.
    fixture.world.connect(PAIR).await.unwrap();
    fixture.world.flood_repeating(10, 11).await.unwrap();
    fixture.world.connect(OTHER).await.unwrap();
    fixture.world.flood_repeating(10, 21).await.unwrap();
    app.until("both pairs on AAP", |s| {
        s.is_connected(PAIR) && s.is_connected(OTHER)
    })
    .await;
}

/// A device signaling its services over and over never holds up a pair's
/// attachment. An advertiser can list the AirPods service and a new one in
/// every advertisement, and bluetoothd signals the device's whole list each
/// time, AirPods service and all. Regression: each signal was queued for
/// attachment, which asks BlueZ about the device three times for each. Ten
/// thousand, signaled in a couple of seconds, took it nearly a minute, and a
/// pair connecting after them waited behind them all.
///
/// The fake BlueZ lists services only on paired devices, so the advertiser is
/// one paired with it that never connects. To attachment that is the same:
/// whether the device is connected is all it asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_resignaling_its_services_does_not_delay_attachment() {
    const ADVERTISER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x04]);
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let bluez = fixture.world.bluez();
    bluez
        .add_device(HCI0, ADVERTISER, "Advertiser")
        .await
        .unwrap();
    let mut app = fixture.start_app().await;

    // Taken out and listed again: only a list with the AirPods service passes
    // the watch's filter.
    for _ in 0..10_000 {
        bluez.set_airpods_service(ADVERTISER, false).await.unwrap();
        bluez.set_airpods_service(ADVERTISER, true).await.unwrap();
    }
    app.attach(PAIR).await;
}

/// An adapter powering off and on - GNOME's Bluetooth switch, rfkill, a
/// suspend - ends discovery there and disconnects its pairs, as bluetoothd
/// does. Back on, discovery resumes: a pair in range is heard again, with
/// levels only a fresh advertisement brings. Regression (#50): discovery was
/// started once, at startup, and after the first power cycle nothing was
/// heard over BLE. Discovery and the pair heard again are what the fix
/// brings; the connected pair getting its battery back as it reconnects is
/// checked along the way, but held before it too, as bluetoothd keeps an
/// adapter's providers while it is off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_resumes_after_an_adapter_power_cycle() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    fixture.seed_keys(&[PAIR, OTHER]).await;
    let mut app = fixture.start_app().await;
    app.attach(OTHER).await;
    let battery = HashMap::from([(OTHER, lowest(OTHER_LEVELS))]);
    fixture.until_batteries(&battery).await;
    app.until("the pair in range over BLE", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    fixture.world.set_powered(HCI0, false).await.unwrap();
    app.until("the connected pair off AAP", |s| !s.is_connected(OTHER))
        .await;
    fixture.until_batteries(&HashMap::new()).await;
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;

    fixture.world.set_powered(HCI0, true).await.unwrap();
    fixture.until_discovering(HCI0).await;
    app.until("the pair in range heard again", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
    app.attach(OTHER).await;
    fixture.until_batteries(&battery).await;
}

/// An adapter off when the app starts is not scanned, and is once it powers
/// on; it has its battery provider all along, as bluetoothd keeps providers
/// while an adapter is off. Regression (#50): an adapter off at startup was
/// left out, and nothing noticed it powering on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adapter_off_at_startup_is_scanned_once_it_powers_on() {
    let fixture =
        Fixture::with_adapters(&[(HCI0, false)], &[(HCI0, PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    let bluez = fixture.world.bluez();
    assert!(!bluez.is_discovering(HCI0), "discovering while off");
    fixture.until_provider_on(HCI0).await;

    fixture.world.set_powered(HCI0, true).await.unwrap();
    fixture.until_discovering(HCI0).await;
    app.until("a BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;
    app.attach(PAIR).await;
    fixture
        .until_batteries_on(HCI0, &HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
}

/// A USB dongle plugged in while the app runs is scanned and gets a battery
/// provider, so a pair connected through it gets its battery from that
/// adapter's manager. Unplugged, it takes its pair's link and battery along,
/// and the other adapter's pair keeps its own; plugged in again, it gets a
/// provider again. Regression (#50): adapters were found at startup only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adapter_plugged_in_gets_a_provider_and_unplugged_takes_it_along() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    let first = HashMap::from([(PAIR, lowest(PAIR_LEVELS))]);
    fixture.until_batteries_on(HCI0, &first).await;

    let world = &fixture.world;
    world.add_adapter(HCI1, true).await.unwrap();
    world
        .add_pods_on(HCI1, OTHER, PRO_2_USB_C, "Fake AirPods")
        .await
        .unwrap();
    fixture.set_levels(OTHER, OTHER_LEVELS).await;
    fixture.until_discovering(HCI1).await;
    app.attach(OTHER).await;
    let second = HashMap::from([(OTHER, lowest(OTHER_LEVELS))]);
    fixture.until_batteries_on(HCI1, &second).await;

    world.remove_adapter(HCI1).await.unwrap();
    app.until("the pair on it off AAP", |s| !s.is_connected(OTHER))
        .await;
    fixture.until_batteries(&first).await;
    assert!(app.updates.borrow().is_connected(PAIR), "the other's link");

    world.add_adapter(HCI1, true).await.unwrap();
    fixture.until_provider_on(HCI1).await;
    app.attach(OTHER).await;
    fixture.until_batteries_on(HCI1, &second).await;
    fixture.until_batteries_on(HCI0, &first).await;
}

/// bluetoothd restarting forgets every client's discovery and battery
/// providers, and says so only by leaving the bus and coming back: its
/// adapters, and the pair's connection, are as they were. The app registers
/// its provider again, so the connected pair's battery is back without a
/// reconnection, and scans again, so the pair in range is heard. Regression
/// (#50): nothing watched for it, and both stayed gone until the app was
/// restarted, with nothing in its log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bluez_restart_brings_the_provider_and_discovery_back() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    fixture.seed_keys(&[PAIR, OTHER]).await;
    let mut app = fixture.start_app().await;
    app.attach(OTHER).await;
    let battery = HashMap::from([(OTHER, lowest(OTHER_LEVELS))]);
    fixture.until_batteries(&battery).await;
    app.until("the pair in range over BLE", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    fixture.world.restart_bluez().await.unwrap();
    assert!(fixture.world.batteries().is_empty(), "kept by the restart");
    fixture.until_batteries(&battery).await;
    fixture.until_provider_on(HCI0).await;
    fixture.until_discovering(HCI0).await;
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;
    app.until("the pair in range heard again", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
    assert!(app.updates.borrow().is_connected(OTHER), "AAP went");
}

/// bluetoothd killed leaves its discovery running in the controller for
/// seconds, and the new one's own start is refused as busy: it answers the
/// app's `StartDiscovery` with `InProgress`, as it answers a client that has
/// discovery already, and does not take the app on. The app asks again until
/// the controller is free, and the pair in range is heard again without the
/// app restarting. Regression (#123): `InProgress` was taken as started, and
/// nothing was scanned until the app restarted, with nothing in its log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_resumes_after_a_bluez_restart_found_the_adapter_busy() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;
    app.until("the pair in range over BLE", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    let bluez = fixture.world.bluez();
    bluez.set_discovery_busy(HCI0, true);
    fixture.world.restart_bluez().await.unwrap();
    // The app's start, and the one after it stopped whatever it had.
    eventually("the app to ask twice for discovery", move || async move {
        bluez.busy_refusals(HCI0) >= 2
    })
    .await;
    assert!(!bluez.is_discovering(HCI0), "taken on while busy");

    bluez.set_discovery_busy(HCI0, false);
    fixture.until_discovering(HCI0).await;
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;
    app.until("the pair in range heard again", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
}

/// A bluetoothd restart under systemd, where `org.bluez` is activated: the
/// old one leaves, the app asks for the adapters straight away, and the call
/// waits for the new one, which answers it - before the bus says it is
/// there. The app reads the new bluetoothd twice, once for each change of
/// owner, and the second read changes nothing: the provider registered after
/// the first stands, and the connected pair keeps its battery. Regression
/// (rules review of #121): each read gave new tokens, the provider was
/// registered again, BlueZ refused it as registered already, and GNOME
/// Settings lost the battery for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bluez_restart_read_twice_keeps_the_battery() {
    let bus = PrivateBus::activating("org.bluez").expect("dbus-daemon starts");
    let fixture = Fixture::on_bus(
        bus,
        &[(HCI0, true)],
        &[(HCI0, OTHER, PRO_2_USB_C, OTHER_LEVELS)],
    )
    .await;
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let reads = reads_by(&fixture.bus, &attachment).await;
    let mut app = fixture.start_app_on(attachment).await;
    app.attach(OTHER).await;
    let battery = HashMap::from([(OTHER, lowest(OTHER_LEVELS))]);
    fixture.until_batteries(&battery).await;

    let bluez = fixture.world.bluez();
    let before = reads();
    bluez.stop().await.unwrap();
    // Attachment's first read, held until the new one takes the name.
    let bus = &fixture.bus;
    let reads = &reads;
    eventually(
        "attachment to read while BlueZ is away",
        move || async move { bus.activations() > 0 && reads() > before },
    )
    .await;
    bluez.start(bus.address()).await.unwrap();

    // That read changes the adapters - the provider is registered, the names
    // read and the pairs swept, two reads more - and the second change of
    // owner brings a fourth. Nothing else here reads.
    eventually(
        "attachment to read the new BlueZ again",
        move || async move { reads() >= before + 4 },
    )
    .await;
    // An adapter plugged in now is taken in after what the second read
    // brought: once it has a provider, attachment has answered that.
    fixture.world.add_adapter(HCI1, true).await.unwrap();
    fixture.until_provider_on(HCI1).await;
    fixture.until_batteries(&battery).await;
    assert_eq!(bluez.providers_on(HCI0), 1);
}

/// Two LinuxPods on one system bus - two users logged in, each with the app
/// starting with their session - each get a battery provider of their own,
/// at a root of their own: bluetoothd tells providers apart by root alone.
/// Once the first has gone, the second's batteries are the ones GNOME
/// Settings shows. Regression (rules review of #121): both served the same
/// root, the second took BlueZ's refusal of it as registered already for its
/// own registration, and was never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_apps_on_one_bus_each_register_a_provider() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut first = fixture.start_app().await;
    first.attach(PAIR).await;
    let battery = HashMap::from([(PAIR, lowest(PAIR_LEVELS))]);
    fixture.until_batteries(&battery).await;

    let mut second = fixture.start_app().await;
    let bluez = fixture.world.bluez();
    eventually("a provider for each app", move || async move {
        bluez.providers_on(HCI0) == 2
    })
    .await;

    // The second's battery for the pair was turned away while the first's
    // stood, as bluetoothd turns away a second; its next one is taken. Not
    // looked at before that: dropped, the first takes its AAP link along, and
    // the second, trying again, may link the pair and offer its battery
    // before bluetoothd has forgotten the first's, or after.
    drop(first);
    fixture.until_provider_on(HCI0).await;
    second.detach(PAIR).await;
    fixture.until_batteries(&HashMap::new()).await;
    second.attach(PAIR).await;
    fixture.until_batteries(&battery).await;
}

/// Counts the `GetManagedObjects` calls `conn` makes on `bus`, as a monitor
/// sees them - a call waiting for BlueZ to be activated among them, which
/// BlueZ itself does not see until it is there. Returns the count so far, on
/// each call.
async fn reads_by(bus: &PrivateBus, conn: &Connection) -> impl Fn() -> usize + use<> {
    calls_by(
        bus,
        conn,
        Box::new(|header| header.member().is_some_and(|m| m == "GetManagedObjects")),
    )
    .await
}

/// Which calls [`calls_by`] counts, or signals [`signals_by`].
type Pick = Box<dyn Fn(&zbus::message::Header<'_>) -> bool + Send>;

/// Counts the method calls `conn` makes on `bus` that `pick` picks out, as a
/// monitor sees them: in the order `conn` made them. Returns the count so
/// far, on each call.
async fn calls_by(bus: &PrivateBus, conn: &Connection, pick: Pick) -> impl Fn() -> usize + use<> {
    messages_by(bus, conn, zbus::message::Type::MethodCall, pick).await
}

/// [`calls_by`], for the signals `conn` sends.
async fn signals_by(bus: &PrivateBus, conn: &Connection, pick: Pick) -> impl Fn() -> usize + use<> {
    messages_by(bus, conn, zbus::message::Type::Signal, pick).await
}

/// Counts the messages of type `kind` that `conn` sends on `bus` and `pick`
/// picks out - see [`calls_by`].
async fn messages_by(
    bus: &PrivateBus,
    conn: &Connection,
    kind: zbus::message::Type,
    pick: Pick,
) -> impl Fn() -> usize + use<> {
    let caller = conn.unique_name().unwrap().to_string();
    let monitor = bus.connect().await.unwrap();
    let mut messages = zbus::MessageStream::from(&monitor);
    let rule = zbus::MatchRule::builder().msg_type(kind).build();
    zbus::fdo::MonitoringProxy::new(&monitor)
        .await
        .unwrap()
        .become_monitor(&[rule], 0)
        .await
        .unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();
    tokio::spawn(async move {
        let _monitor = monitor;
        while let Some(Ok(msg)) = futures_util::StreamExt::next(&mut messages).await {
            let header = msg.header();
            if header.sender().is_some_and(|s| s.as_str() == caller) && pick(&header) {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });
    move || count.load(std::sync::atomic::Ordering::SeqCst)
}

/// Picks the calls on the object at `path`.
fn on_path(path: zbus::zvariant::OwnedObjectPath) -> Pick {
    Box::new(move |header| header.path().is_some_and(|p| p.as_str() == path.as_str()))
}

/// An adapter flapping - powered off and on over and over, faster than the
/// app can answer each change - leaves discovery running once it settles,
/// and attachment answering: a pair connecting then is attached and given
/// its battery, and heard over BLE once it disconnects. The fix (#50) is what
/// brings discovery back; that the flapping is coalesced rather than queued
/// is the adapter watch's unit tests' to show, as nothing here can tell a
/// backlog worked through from none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_resumes_after_a_flap_storm() {
    const FLAPS: usize = 10_000;
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    let mut app = fixture.start_app().await;

    for _ in 0..FLAPS {
        fixture.world.set_powered(HCI0, false).await.unwrap();
        fixture.world.set_powered(HCI0, true).await.unwrap();
    }
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;
    app.attach(PAIR).await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(changed))]))
        .await;
    fixture.until_discovering(HCI0).await;
    app.detach(PAIR).await;
    app.until("a BLE reading after the flapping", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
}

/// A pair that hangs up AAP while BlueZ keeps it connected - the pods
/// dropping the connection on their own - is linked again: its battery comes
/// from AAP again, in the app and in GNOME Settings, and its keys are asked
/// for again, as on every connection. Regression (#49): the link stayed down,
/// and the pair on BLE, until it was disconnected and connected again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_that_hangs_up_aap_while_connected_is_linked_again() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    app.until("the key and an AAP reading", |s| {
        s.known_keys == [PAIR] && levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    fixture.until_key_requests(PAIR, 1).await;

    fixture.world.hang_up(PAIR).await.unwrap();
    // What only the pair's next connection can bring over AAP.
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;
    fixture.until_attempts(PAIR, 2).await;
    app.until("an AAP reading on the new link", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(changed)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(changed))]))
        .await;
    fixture.until_key_requests(PAIR, 2).await;
}

/// An AAP connection the pair turns away is tried again, after a wait that
/// grows with each failure, until one is taken. Regression (#49): a failed
/// connect was never tried again, and the pair stayed on BLE for as long as
/// it was connected. A connection turned away once it is taken counts as a
/// link that went during setup (#123), logged at debug rather than warned
/// of, and the wait grows all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_aap_connection_is_tried_again() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.refuse_aap = 2)
        .await
        .unwrap();
    let mut app = fixture.start_app().await;

    app.attach(PAIR).await;
    app.until("an AAP reading", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    assert_eq!(
        fixture.aap_attempts(PAIR).await,
        3,
        "two refused, one taken"
    );
}

/// While one pair's AAP setup hangs - the pods take the connection and never
/// answer the handshake - a second pair connecting is attached, linked and
/// given its battery without waiting for the first: attachment never waits
/// for an AAP setup. The first, once it answers, is linked on a later try.
/// Regression (#49): attachment waited out each setup in turn, so the second
/// pair waited for the first one's handshake to time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_aap_setup_does_not_hold_up_another_pair() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.silent_handshake = true)
        .await
        .unwrap();
    let mut app = fixture.start_app().await;

    fixture.world.connect(PAIR).await.unwrap();
    fixture.until_attempts(PAIR, 1).await;
    app.attach(OTHER).await;
    app.until("the other pair's AAP reading", |s| {
        levels(s, OTHER, DataSource::Aap) == Some(OTHER_LEVELS)
    })
    .await;
    fixture
        .until_batteries(&HashMap::from([(OTHER, lowest(OTHER_LEVELS))]))
        .await;
    assert!(
        fixture.has_aap(PAIR).await && fixture.aap_attempts(PAIR).await == 1,
        "the other pair waited for the first one's handshake to time out"
    );

    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.silent_handshake = false)
        .await
        .unwrap();
    app.until("the first pair's AAP reading, on a later try", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    assert!(fixture.aap_attempts(PAIR).await >= 2);
}

/// Detaching a pair stops its AAP attempts, whether it goes while a setup
/// hangs or while waiting to try again: the hanging connection is closed at
/// once, nothing connects to the pair after, and no link is left. A connect
/// after detaching would do worse than waste a try: an L2CAP connect brings
/// the pair's Bluetooth link back up, reconnecting AirPods the user has just
/// disconnected.
///
/// The first detach is BlueZ's alone: the pods keep their silent connection
/// open, so only the app can close it, ahead of the handshake's timeout. After
/// the second, BlueZ shows the pair connected again, as a device that is not
/// AirPods, which attachment leaves alone: a supervisor still running would
/// find it connected and try it, where a check of BlueZ alone would not
/// stop it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detaching_a_pair_stops_its_aap_attempts() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    // For a battery, from BLE, that tells when the pair is attached.
    fixture.seed_keys(&[PAIR]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.silent_handshake = true)
        .await
        .unwrap();
    let app = fixture.start_app().await;
    let fixture = &fixture;

    // During a setup that hangs.
    fixture.world.connect(PAIR).await.unwrap();
    fixture.until_attempts(PAIR, 1).await;
    let opened = Instant::now();
    fixture
        .world
        .bluez()
        .set_connected(PAIR, false)
        .await
        .unwrap();
    eventually("the hanging connection closed", move || async move {
        !fixture.has_aap(PAIR).await
    })
    .await;
    assert!(
        opened.elapsed() < HANDSHAKE_TIMEOUT - Duration::from_secs(1),
        "closed by the handshake's timeout, not by the detach"
    );
    // Answered from here on, so that a connection would show as a link.
    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.silent_handshake = false)
        .await
        .unwrap();
    tokio::time::sleep(QUIET).await;
    assert_eq!(fixture.aap_attempts(PAIR).await, 1, "tried again");
    assert!(!app.updates.borrow().is_connected(PAIR), "a link left");

    // While waiting to try again.
    fixture
        .world
        .with_pods(PAIR, |pods| pods.quirks.refuse_aap = u32::MAX)
        .await
        .unwrap();
    fixture.world.connect(PAIR).await.unwrap();
    let battery = HashMap::from([(PAIR, lowest(PAIR_LEVELS))]);
    fixture.until_batteries(&battery).await;
    fixture.until_attempts(PAIR, 3).await;
    fixture.world.disconnect(PAIR).await.unwrap();
    // Withdrawn once the pair is detached, its supervisor stopped.
    fixture.until_batteries(&HashMap::new()).await;
    let bluez = fixture.world.bluez();
    bluez.set_airpods_service(PAIR, false).await.unwrap();
    bluez.set_connected(PAIR, true).await.unwrap();
    let attempts = fixture.aap_attempts(PAIR).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(fixture.aap_attempts(PAIR).await, attempts, "tried again");
    assert!(!app.updates.borrow().is_connected(PAIR), "a link left");
}

/// Across a hang-up and the link coming back, the pair's GNOME Settings
/// battery stays exported, never withdrawn and exported again: its BLE
/// reading shows from when the old link goes until the new one's first
/// battery packet. The pods take half a second over that packet, as one pair
/// took about a second. Regression (#62, #49): the BLE reading was hidden as
/// soon as a link was up, ahead of its first packet, and the battery went
/// missing in GNOME Settings meanwhile, on every reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnect_keeps_the_battery_exported() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    fixture.seed_keys(&[PAIR]).await;
    fixture
        .world
        .with_pods(PAIR, |pods| {
            pods.quirks.report_delay = Duration::from_millis(500);
        })
        .await
        .unwrap();
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    // An AAP reading takes its connection state from the cached
    // advertisement, so once it has one, an advertisement is cached.
    app.until("an AAP reading over a cached advertisement", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
            && s.states[&PAIR].connection_state.is_some()
    })
    .await;
    let battery = HashMap::from([(PAIR, lowest(PAIR_LEVELS))]);
    fixture.until_batteries(&battery).await;
    let bluez = fixture.world.bluez();
    assert!(bluez.withdrawn_batteries().is_empty());

    fixture.world.hang_up(PAIR).await.unwrap();
    // A second connection means the first link has gone.
    fixture.until_attempts(PAIR, 2).await;
    app.until("an AAP reading on the new link", |s| {
        levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
    // Signaled after anything the reconnect brought GNOME Settings, so once
    // it shows, a withdrawal would have too.
    let changed = [PAIR_LEVELS[0] - 5, PAIR_LEVELS[1] - 3, PAIR_LEVELS[2]];
    fixture.set_levels(PAIR, changed).await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(changed))]))
        .await;
    assert!(
        bluez.withdrawn_batteries().is_empty(),
        "withdrawn across the reconnect: {:?}",
        bluez.withdrawn_batteries()
    );
}

/// A services change on a device that is neither attached nor connected
/// costs attachment one call to BlueZ, on that device: whether it is
/// connected, asked first. Regression (rules review of #102): its alias and
/// every device's names were read ahead of that, so an advertiser whose
/// services changed at every address it rotated to cost three calls each.
///
/// The pair, connected before the app starts but not yet listing the AirPods
/// service, is attached by a services change of its own, signaled after the
/// device's: services changes are handled in the order they came, so once
/// attachment calls on the pair, its calls on the device are all in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_services_change_on_a_disconnected_device_costs_one_call() {
    const DEVICE: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x04]);
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let bluez = fixture.world.bluez();
    bluez.add_device(HCI0, DEVICE, "Device").await.unwrap();
    bluez.set_airpods_service(DEVICE, false).await.unwrap();
    bluez.set_airpods_service(PAIR, false).await.unwrap();
    fixture.world.connect(PAIR).await.unwrap();
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let on_device = calls_by(
        &fixture.bus,
        &attachment,
        on_path(device_path(HCI0, DEVICE)),
    )
    .await;
    let on_pair = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, PAIR))).await;
    let mut app = fixture.start_app_on(attachment).await;
    assert!(
        app.updates.borrow().connected_macs.is_empty(),
        "attached by the sweep"
    );

    bluez.set_airpods_service(DEVICE, true).await.unwrap();
    bluez.set_airpods_service(PAIR, true).await.unwrap();
    app.until("the pair on AAP", |s| s.is_connected(PAIR)).await;
    let on_pair = &on_pair;
    eventually("attachment's calls on the pair", move || async move {
        on_pair() > 0
    })
    .await;
    assert_eq!(on_device(), 1, "calls on the device");
}

/// While BlueZ is off the bus, a pair whose link goes is not asked about: its
/// supervisor waits for BlueZ to be back before it asks whether the pair is
/// still connected, and links it again then. Asked meanwhile, a bus that
/// activates bluetoothd - systemd's - would start it again, undoing a
/// `systemctl stop bluetooth` within a second or two. Regression (rules
/// review of #122): the supervisor asked BlueZ at every wait, though
/// attachment asks it nothing while it is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_whose_link_goes_while_bluez_is_away_waits_for_it() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let on_pair = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, PAIR))).await;
    let mut app = fixture.start_app_on(attachment).await;
    app.attach(PAIR).await;

    let bluez = fixture.world.bluez();
    bluez.stop().await.unwrap();
    fixture.world.hang_up(PAIR).await.unwrap();
    app.until("the link gone", |s| !s.is_connected(PAIR)).await;
    let before = on_pair();
    tokio::time::sleep(QUIET).await;
    assert_eq!(on_pair(), before, "asked BlueZ while it was away");

    bluez.start(fixture.bus.address()).await.unwrap();
    app.until("the pair linked again", |s| s.is_connected(PAIR))
        .await;
    assert_eq!(fixture.aap_attempts(PAIR).await, 2);
}

/// An adapter powering off while a pair is linked takes the link and
/// detaches the pair, as #121 has it, and nothing tries AAP while the adapter
/// is off. Powered on, and the pair connected again, it is linked again, with
/// one attempt: the count catches any more, such as from a supervisor left
/// over from before the power-off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adapter_powered_off_stops_aap_until_the_pair_connects_again() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let mut app = fixture.start_app().await;
    app.attach(PAIR).await;
    let battery = HashMap::from([(PAIR, lowest(PAIR_LEVELS))]);
    fixture.until_batteries(&battery).await;

    fixture.world.set_powered(HCI0, false).await.unwrap();
    app.until("the pair off AAP", |s| !s.is_connected(PAIR))
        .await;
    fixture.until_batteries(&HashMap::new()).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(fixture.aap_attempts(PAIR).await, 1, "tried while off");

    fixture.world.set_powered(HCI0, true).await.unwrap();
    app.attach(PAIR).await;
    fixture.until_batteries(&battery).await;
    assert_eq!(fixture.aap_attempts(PAIR).await, 2);
}

/// Signals on `forger`, a client on the bus that is not BlueZ, that the pair
/// at `mac` on `hci0` changed `Connected` to `connected`: the signal
/// bluetoothd sends, on the pair's path, but not from bluetoothd. The bus
/// lets any client send it, and the connection watch's rule names no sender.
async fn forge_connected(forger: &Connection, mac: Address, connected: bool) {
    let changed = HashMap::from([("Connected", zbus::zvariant::Value::from(connected))]);
    forger
        .emit_signal(
            None::<&str>,
            device_path(HCI0, mac),
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", changed, Vec::<String>::new()),
        )
        .await
        .unwrap();
}

/// A `Connected=true` from anyone but BlueZ, on the path of a pair that is
/// paired and disconnected, attaches nothing: attachment asks BlueZ, which
/// reports the pair disconnected, and nothing pages the pair with an AAP
/// connection. Regression (#132): the event was taken as given, the pair
/// attached, and its supervisor's first connect - which asks BlueZ nothing,
/// as attaching has asked - reconnected AirPods the user had disconnected.
///
/// Attachment's call on the pair's path shows it has taken the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_connection_attaches_nothing() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let on_pair = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, PAIR))).await;
    let app = fixture.start_app_on(attachment).await;
    let forger = fixture.bus.connect().await.unwrap();

    forge_connected(&forger, PAIR, true).await;
    let on_pair = &on_pair;
    eventually("attachment to ask about the pair", move || async move {
        on_pair() > 0
    })
    .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(on_pair(), 1, "calls on the pair");
    assert_eq!(fixture.aap_attempts(PAIR).await, 0, "AAP to the pair");
    let connected = fixture.world.with_pods(PAIR, |pods| pods.is_connected());
    assert!(!connected.await.unwrap(), "the pair was reconnected");
    assert!(!app.updates.borrow().is_connected(PAIR), "a link");
}

/// A `Connected=false` from anyone but BlueZ, on the path of a pair that is
/// attached and connected, detaches nothing: attachment asks BlueZ, which
/// reports the pair connected, and the pair keeps its link and its GNOME
/// Settings battery. Regression (#132): the event was taken as given, the
/// pair detached and its battery withdrawn.
///
/// The other pair, connecting once attachment has asked about the first, is
/// taken after the forged event: once it is on AAP with its battery shown, a
/// detach would have shown too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_disconnection_detaches_nothing() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let on_pair = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, PAIR))).await;
    let mut app = fixture.start_app_on(attachment).await;
    app.attach(PAIR).await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
    let forger = fixture.bus.connect().await.unwrap();

    let before = on_pair();
    forge_connected(&forger, PAIR, false).await;
    let on_pair = &on_pair;
    eventually("attachment to ask about the pair", move || async move {
        on_pair() > before
    })
    .await;
    app.attach(OTHER).await;
    let world = &fixture.world;
    eventually("the other pair's battery", move || async move {
        world.batteries().contains_key(&OTHER)
    })
    .await;
    assert!(app.updates.borrow().is_connected(PAIR), "the link went");
    assert_eq!(fixture.aap_attempts(PAIR).await, 1);
    assert_eq!(
        fixture.world.batteries(),
        HashMap::from([(PAIR, lowest(PAIR_LEVELS)), (OTHER, lowest(OTHER_LEVELS)),])
    );
    let bluez = fixture.world.bluez();
    assert!(
        bluez.withdrawn_batteries().is_empty(),
        "withdrawn: {:?}",
        bluez.withdrawn_batteries()
    );
}

/// While BlueZ is off the bus, a `Connected` change is not its own - forged,
/// or from the bluetoothd that left - and attachment neither asks BlueZ
/// about it nor acts on it: on a bus that activates bluetoothd, systemd's,
/// asking would start it again. A connection on a pair's path costs no call,
/// and a disconnection on the attached pair's detaches nothing, though BlueZ
/// is not there to say the pair is still connected.
///
/// Attachment withdraws the pair's battery as it learns BlueZ has left, in
/// the loop that takes connection events: once the battery is withdrawn,
/// every event after is taken knowing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_event_while_bluez_is_away_is_not_asked_about() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let attachment = fixture.bus.connect_for_providers().await.unwrap();
    let withdrawn = signals_by(
        &fixture.bus,
        &attachment,
        Box::new(|header| header.member().is_some_and(|m| m == "InterfacesRemoved")),
    )
    .await;
    let on_pair = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, PAIR))).await;
    let on_other = calls_by(&fixture.bus, &attachment, on_path(device_path(HCI0, OTHER))).await;
    let mut app = fixture.start_app_on(attachment).await;
    app.attach(PAIR).await;
    fixture
        .until_batteries(&HashMap::from([(PAIR, lowest(PAIR_LEVELS))]))
        .await;
    let forger = fixture.bus.connect().await.unwrap();

    fixture.world.bluez().stop().await.unwrap();
    let withdrawn = &withdrawn;
    eventually(
        "attachment to withdraw the battery as BlueZ leaves",
        move || async move { withdrawn() > 0 },
    )
    .await;
    let (pair_before, other_before) = (on_pair(), on_other());
    forge_connected(&forger, OTHER, true).await;
    forge_connected(&forger, PAIR, false).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(on_other(), other_before, "asked BlueZ about the other pair");
    assert_eq!(on_pair(), pair_before, "asked BlueZ about the pair");
    assert_eq!(
        fixture.aap_attempts(OTHER).await,
        0,
        "AAP to the other pair"
    );
    assert!(app.updates.borrow().is_connected(PAIR), "the link went");
}
