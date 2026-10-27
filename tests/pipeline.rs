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
//! Time runs at real speed: each pair advertises once a second, from an
//! address that rotates every ten. Tests wait for what they expect to see,
//! under [`TIMEOUT`]. The 30-minute BLE cache expiry stays with the
//! coordinator's unit tests.
//!
//! Every key is generated at runtime, by the fake pairs; none is in the source.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bluer::Address;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zbus::Connection;

use linuxpods::aap::NoiseMode;
use linuxpods::aap::noise::{allow_off_packet, set_packet};
use linuxpods::attachment::Attachment;
use linuxpods::ble::decrypt_for_device;
use linuxpods::ble::scanner::{Advertisement, Scanner};
use linuxpods::bluez::{BatteryProvider, ProviderConnection};
use linuxpods::harness::bluez::device_path;
use linuxpods::harness::pods::{PRO_2_USB_C, PRO_3, Part};
use linuxpods::harness::{PrivateBus, World};
use linuxpods::keystore::Keystore;
use linuxpods::podstate::{Coordinator, DataSource, Snapshot};

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

    fn keystore(&self) -> Keystore {
        Keystore::with_dir(self.data.path().to_path_buf()).unwrap()
    }

    /// Saves the pairs' keys where the app looks for them, as though each had
    /// connected in an earlier session.
    async fn seed_keys(&self, macs: &[Address]) {
        let mut keys = HashMap::new();
        for &mac in macs {
            keys.insert(mac, self.enc_key(mac).await);
        }
        self.keystore().save(&keys).unwrap();
    }

    /// Waits until the key file holds `expected` and nothing else. The
    /// coordinator saves a key after it broadcasts it, and nothing tells when
    /// the save is done.
    async fn until_stored(&self, expected: &HashMap<Address, Vec<u8>>) {
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
        let coordinator = Coordinator::with_aap_socket_dir(
            Some(self.keystore()),
            self.sockets.path().to_path_buf(),
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

        App {
            world: self.world.clone(),
            coordinator,
            updates,
            handled: handled_rx,
            listening,
            attaching,
        }
    }
}

/// The app as `main.rs` wires it, less the UI and tray: the real scanner and
/// attachment on the test's bus, feeding the real coordinator, which keeps its
/// keys in the fixture's keystore and opens AAP to the pairs' sockets.
struct App {
    world: World,
    coordinator: Arc<Coordinator>,
    /// What the window, tray and battery provider see.
    updates: watch::Receiver<Snapshot>,
    /// Every advertisement the coordinator has handled, once it has.
    handled: mpsc::UnboundedReceiver<Advertisement>,
    /// The scanner's task.
    listening: JoinHandle<()>,
    /// Attachment's task.
    attaching: JoinHandle<()>,
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
        // Cloned out: the borrow is a read lock every broadcast waits for.
        let found = match timeout(TIMEOUT, self.updates.wait_for(done)).await {
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
        mut pick: impl FnMut(&Advertisement) -> bool,
    ) {
        let mut seen = 0;
        let waited = timeout(TIMEOUT, async {
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
        .until_stored(&HashMap::from([(PAIR, key.clone())]))
        .await;

    app.detach(PAIR).await;
    app.until("an exact BLE reading", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
    })
    .await;

    // The next session, which loads the key from the keystore.
    drop(app);
    let mut app = fixture.start_app().await;
    let snapshot = app
        .until("an exact BLE reading in the next session", |s| {
            levels(s, PAIR, DataSource::Ble) == Some(PAIR_LEVELS)
        })
        .await;
    assert_eq!(snapshot.known_keys, [PAIR]);
    assert!(snapshot.connected_macs.is_empty());
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
        (PAIR, fixture.enc_key(PAIR).await),
        (OTHER, fixture.enc_key(OTHER).await),
    ]);
    assert_ne!(keys[&PAIR], keys[&OTHER]);
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
        .until_stored(&HashMap::from([(PAIR, new.clone())]))
        .await;
    app.detach(PAIR).await;
    app.until("a BLE reading with the new key", |s| {
        levels(s, PAIR, DataSource::Ble) == Some(changed)
    })
    .await;
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
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancelling))
    })
    .await;

    let modes = [
        NoiseMode::Transparency,
        NoiseMode::Adaptive,
        NoiseMode::Off,
        NoiseMode::NoiseCancelling,
    ];
    for mode in modes {
        app.coordinator
            .set_noise_control(PAIR, mode)
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
        set_packet(NoiseMode::NoiseCancelling),
    ]
    .map(|packet| packet.to_vec());
    assert_eq!(settings, expected);
}

/// After Off, a report of the mode it replaced does not bring that mode back:
/// in the #71 capture the pair reported Noise Cancelling 0.33 s after Off
/// was set from it, and Off itself only at 1.4 s, and the window showed Noise
/// Cancelling for the second in between (#113). The fake pair does the same.
///
/// Every snapshot from the command on is watched, until a battery change made
/// once the echo is out has arrived: it comes over the same connection, after
/// the stale report and the echo, so by then both have been read. The stale
/// report comes well inside the 2 s the app holds it off for, and the pair
/// reports Noise Cancelling for a whole second before the echo: a snapshot
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
            .is_some_and(|state| state.noise_mode == Some(NoiseMode::NoiseCancelling))
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
        .set_noise_control(PAIR, NoiseMode::Off)
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
        [Some(NoiseMode::NoiseCancelling), Some(NoiseMode::Off)],
        "Off, and nothing after it"
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
        HashMap::from([(PAIR, key)])
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
    let mut pods = linuxpods::harness::pods::Pods::new(PRO_3, PAIR);
    let [left, right, case] = PAIR_LEVELS;
    pods.set_battery(Part::Left, Some(left));
    pods.set_battery(Part::Right, Some(right));
    pods.set_battery(Part::Case, Some(case));
    fixture
        .keystore()
        .save(&HashMap::from([(PAIR, pods.keys().enc_key.to_vec())]))
        .unwrap();
    let mut app = fixture.start_app().await;

    let advert = pods.advertisement(Duration::ZERO);
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
/// lists the AirPods service, so it is not recognised as it connects. It is
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
        "attached before it could be recognised"
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
/// advertisement is signalled twice, from a device of its address under each:
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
    // first is a `PropertiesChanged`, the one signal the connection watch
    // subscribes to: ten advertisers bring it 100 before the second pair's
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

/// A device signalling its services over and over never holds up a pair's
/// attachment. An advertiser can list the AirPods service and a new one in
/// every advertisement, and bluetoothd signals the device's whole list each
/// time, AirPods service and all. Regression: each signal was queued for
/// attachment, which asks BlueZ about the device three times for each. Ten
/// thousand, signalled in a couple of seconds, took it nearly a minute, and a
/// pair connecting after them waited behind them all.
///
/// The fake BlueZ lists services only on paired devices, so the advertiser is
/// one paired with it that never connects. To attachment that is the same:
/// whether the device is connected is all it asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_resignalling_its_services_does_not_delay_attachment() {
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

/// Which calls [`calls_by`] counts.
type Pick = Box<dyn Fn(&zbus::message::Header<'_>) -> bool + Send>;

/// Counts the method calls `conn` makes on `bus` that `pick` picks out, as a
/// monitor sees them: in the order `conn` made them. Returns the count so
/// far, on each call.
async fn calls_by(bus: &PrivateBus, conn: &Connection, pick: Pick) -> impl Fn() -> usize + use<> {
    let caller = conn.unique_name().unwrap().to_string();
    let monitor = bus.connect().await.unwrap();
    let mut messages = zbus::MessageStream::from(&monitor);
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .build();
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
    // Signalled after anything the reconnect brought GNOME Settings, so once
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
/// service, is attached by a services change of its own, signalled after the
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
