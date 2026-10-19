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
//! - a [`World`] on it: the fake BlueZ, and pairs of fake AirPods that
//!   advertise through it and answer AAP on a Unix socket each
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
use futures_util::StreamExt;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zbus::Connection;

use linuxpods::aap::NoiseMode;
use linuxpods::aap::noise::{allow_off_packet, set_packet};
use linuxpods::attachment::Attachment;
use linuxpods::ble::decrypt_for_device;
use linuxpods::ble::scanner::{Advertisement, Scanner};
use linuxpods::bluez::BatteryProvider;
use linuxpods::harness::pods::{PRO_2_USB_C, PRO_3, Part};
use linuxpods::harness::{PrivateBus, World};
use linuxpods::keystore::Keystore;
use linuxpods::podstate::{Coordinator, DataSource, Snapshot};

/// The longest any one wait may take, so that a regression fails the test
/// instead of hanging CI. The slowest wait is for a pair's first BLE reading:
/// up to an advertising interval, or two should one go out while the scanner
/// is still subscribing. The rest is headroom for a loaded runner.
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
    /// A published world with `pairs` in it - each a real address, a product
    /// ID and its battery levels - in range and disconnected.
    async fn new(pairs: &[(Address, u16, [u8; 3])]) -> Self {
        init_logs();
        let bus = PrivateBus::start().expect("dbus-daemon starts");
        let sockets = tempfile::Builder::new()
            .prefix("linuxpods-aap")
            .tempdir()
            .unwrap();
        let data = tempfile::tempdir().unwrap();
        let world = World::new(bus.address(), sockets.path()).await.unwrap();
        let fixture = Self {
            world,
            sockets,
            data,
            bus,
        };
        for &(mac, product_id, levels) in pairs {
            fixture
                .world
                .add_pods(mac, product_id, "Fake AirPods")
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

    /// Starts the app on this world - see [`App`].
    async fn start_app(&self) -> App {
        self.start_app_on(self.bus.connect().await.unwrap()).await
    }

    /// [`Fixture::start_app`], with attachment on `bluez`, a connection to the
    /// test's bus that the test may have used first.
    async fn start_app_on(&self, bluez: Connection) -> App {
        let coordinator = Coordinator::with_aap_socket_dir(
            Some(self.keystore()),
            self.sockets.path().to_path_buf(),
        );
        let updates = coordinator.subscribe();

        // As main.rs's ble_task, on the test's bus. The stream borrows the
        // scanner, so the two live in the task together.
        let conn = self.bus.connect().await.unwrap();
        let (ready, started) = oneshot::channel();
        let (handled, handled_rx) = mpsc::unbounded_channel();
        let feed = coordinator.clone();
        let listening = tokio::spawn(async move {
            let scanner = Scanner::with_connection(conn);
            scanner.start_discovery().await.expect("discovery starts");
            let adverts = scanner.advertisements().await.expect("the scanner listens");
            tokio::pin!(adverts);
            let _ = ready.send(());
            while let Some(advert) = adverts.next().await {
                feed.handle_advertisement(advert.data.clone(), advert.ble_mac)
                    .await;
                let _ = handled.send(advert);
            }
        });
        timeout(TIMEOUT, started)
            .await
            .expect("timed out starting the scanner")
            .expect("the scanner started");

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
    // By address: the scanner hears each advertiser twice.
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

/// A pair is attached though BlueZ refuses the battery provider, as it does
/// with no BatteryProviderManager1 at `/org/bluez/hci0` - a BlueZ without one,
/// or an adapter by another name (#51). It gets AAP, if no GNOME Settings
/// battery. Regression: a refused provider stopped attachment altogether, and
/// every pair stayed on BLE, a pair new to the app with no key to be read by.
///
/// Refused here because the app's connection has a provider registered at the
/// same path already.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_battery_provider_leaves_attachment_working() {
    let fixture = Fixture::new(&[(PAIR, PRO_3, PAIR_LEVELS)]).await;
    let bluez = fixture.bus.connect().await.unwrap();
    let _registered = BatteryProvider::with_connection(bluez.clone())
        .await
        .unwrap();
    let mut app = fixture.start_app_on(bluez).await;

    app.attach(PAIR).await;
    app.until("the key and an AAP reading", |s| {
        s.known_keys == [PAIR] && levels(s, PAIR, DataSource::Aap) == Some(PAIR_LEVELS)
    })
    .await;
}

/// Advertisements piling up while attachment is busy - a flood, or a crowd,
/// while a pair's AAP link comes up - never stall it. Regression: they filled
/// the connection watch's queue, zbus stopped reading the connection until
/// there was room, and the reply to attachment's next call to BlueZ never
/// came. No pair was attached or detached again.
///
/// All of it while the first pair's link comes up, which takes half a second,
/// so the second pair's connection is handled after it, with one flood
/// queued ahead of it and another behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_while_a_pair_attaches_does_not_stall_attachment() {
    let fixture = Fixture::new(&[
        (PAIR, PRO_3, PAIR_LEVELS),
        (OTHER, PRO_2_USB_C, OTHER_LEVELS),
    ])
    .await;
    let mut app = fixture.start_app().await;

    fixture.world.connect(PAIR).await.unwrap();
    fixture.world.flood(FLOOD as u32).await.unwrap();
    fixture.world.connect(OTHER).await.unwrap();
    fixture.world.flood(2 * FLOOD as u32).await.unwrap();
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
    bluez.add_device(ADVERTISER, "Advertiser").await.unwrap();
    let mut app = fixture.start_app().await;

    // Taken out and listed again: only a list with the AirPods service passes
    // the watch's filter.
    for _ in 0..10_000 {
        bluez.set_airpods_service(ADVERTISER, false).await.unwrap();
        bluez.set_airpods_service(ADVERTISER, true).await.unwrap();
    }
    app.attach(PAIR).await;
}
