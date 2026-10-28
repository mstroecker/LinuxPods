//! The fake world: pairs of fake AirPods, the fake BlueZ they are seen
//! through, and their AAP sockets, running for the app to meet in place of
//! real ones (#72).
//!
//! The fake BlueZ has the adapters the world is made with: one, `hci0`, unless
//! [`World::with_adapters`] says otherwise. They can be powered off and on,
//! plugged in and out, and the fake BlueZ restarted, at any time (#50). Each
//! pair is a [`Pods`] model at a real address, paired with one of them, and
//! loses its link when its adapter powers off or goes. The world carries out
//! what the model asks for and nothing else:
//!
//! - **BLE.** Every [`ADVERTISING_INTERVAL`] each pair's advertisement goes to
//!   the fake BlueZ, which signals it while the app discovers - from the
//!   random address the model rotates, so a new device object every ten
//!   seconds. Every adapter that discovers hears it, whichever the pair is
//!   paired with, as radios in one room do, so with two adapters each
//!   advertisement is signaled twice, from a device under each.
//! - **AAP.** Each pair listens at `aap::client::unix_socket_path(dir, mac)`,
//!   where the app connects in place of L2CAP. What the app sends goes to
//!   [`Pods::receive`], and [`Pods::poll`] runs after that and after every
//!   change, and again when [`Pods::next_due`] says, for the delayed echo and
//!   the silent pair's hang-up.
//!
//! The world changes in two ways that do the same: in the same process, by
//! the methods of [`World`], which the pipeline tests (#73) use; or over
//! D-Bus, by `io.github.mstroecker.LinuxPods.FakeWorld1` at
//! `/io/github/mstroecker/LinuxPods/FakeWorld`, under the name
//! `io.github.mstroecker.LinuxPods.FakeWorld` on the bus the fake BlueZ is on,
//! for `fakepods`, `scripts/fake-world.sh` and Claude Code:
//!
//! | Method | Does |
//! |---|---|
//! | `AddPods(s mac, q model, s alias)` | Pairs new AirPods with the world's first adapter - model by product ID, such as `0x2027` for Pro 3 - in range and disconnected |
//! | `AddPodsOn(s adapter, s mac, q model, s alias)` | The same, with the adapter called `adapter` |
//! | `AddAdapter(s name, b powered)` | Plugs in an adapter at `/org/bluez/<name>`; one plugged in before has its pairs back, disconnected |
//! | `RemoveAdapter(s name)` | Unplugs it: its pairs disconnect, and its devices and battery providers go |
//! | `SetPowered(s name, b powered)` | Powers it on or off; off, discovery there ends and its pairs disconnect |
//! | `RestartBluez()` | bluetoothd restarting as after a crash, under a new unique name: `NameOwnerChanged` twice, and every client's discovery and battery providers forgotten. Nothing is announced again, as a real one would - see [`FakeBluez::restart`] |
//! | `Connect(s mac)` | Connects them to the host, which the app takes up |
//! | `Disconnect(s mac)` | Disconnects them, which takes the AAP connection along |
//! | `SetBattery(s mac, s part, y level)` | `left`, `right` or `case`; a level above 100 is unknown |
//! | `SetLid(s mac, b open)` | Opens or closes the case |
//! | `Repair(s mac)` | Re-pairs: new keys, sent at the next key request |
//! | `HangUp(s mac)` | Closes the AAP connection from the AirPods' end; the Bluetooth link stays |
//! | `SetQuirk(s mac, s quirk, v value)` | Sets one of [`Quirks`] by its field name, `echo_delay_ms`, `report_delay_ms` and `stale_report_ms` in milliseconds, a `stale_report_ms` of 0 for none |
//! | `Flood(u count)` | That many advertisers no key decrypts, each from an address of its own |
//! | `Received(s mac) → aay` | Every AAP packet the app sent the pair, in order |
//! | `Batteries() → a{sy}` | What GNOME Settings shows: the level a battery provider gave each device, by MAC |
//!
//! A malformed argument is `org.freedesktop.DBus.Error.InvalidArgs`, and a
//! request the world cannot carry out - a MAC it has no pair at, a hang-up
//! with no connection - is `org.freedesktop.DBus.Error.Failed`.
//!
//! Time is real: a pair rotates its address every ten seconds and the fake
//! BlueZ forgets an address after thirty. The model's clock starts with the
//! world.

use std::collections::{BTreeMap, HashMap};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, fdo, interface};

use super::bluez::FakeBluez;
use super::connect_serving;
use super::pods::{Action, Keys, PRO_3, Part, Pods, Quirks};
use crate::aap::client::unix_socket_path;
use crate::ble::decrypt::random_key;

/// The name `FakeWorld1` is served under.
pub const SERVICE: &str = "io.github.mstroecker.LinuxPods.FakeWorld";
pub const PATH: &str = "/io/github/mstroecker/LinuxPods/FakeWorld";
pub const INTERFACE: &str = "io.github.mstroecker.LinuxPods.FakeWorld1";

/// The adapter a world has unless it is made with others: the one bluetoothd
/// puts first.
pub const DEFAULT_ADAPTER: &str = "hci0";

/// How often each pair advertises. Real AirPods do so many times a second;
/// once a second keeps the logs readable and still gives an address ten
/// advertisements before it rotates.
pub const ADVERTISING_INTERVAL: Duration = Duration::from_secs(1);

/// Room for any AAP packet the app sends; the longest is 16 bytes.
const READ_BUF_LEN: usize = 1024;

/// The fake world. Clones share it.
#[derive(Clone)]
pub struct World(Arc<Inner>);

struct Inner {
    bluez: FakeBluez,
    /// The connection `FakeWorld1` is served on - not the fake BlueZ's, whose
    /// ObjectManager at `/` would list it among bluetoothd's objects.
    control: Connection,
    /// The bus it is on, for the connection a restarted fake BlueZ comes
    /// back on.
    bus: String,
    socket_dir: PathBuf,
    /// Where the models' time starts.
    epoch: Instant,
    pairs: Mutex<BTreeMap<Address, Pair>>,
    /// Wakes the clock when a pair may have something new scheduled.
    due: Notify,
    next_link: AtomicU64,
}

struct Pair {
    pods: Pods,
    link: Option<Link>,
    /// AAP connections the app has opened to the pair, turned away or not.
    aap_attempts: usize,
}

/// The app's AAP connection to a pair, as the world holds it.
struct Link {
    /// Tells the connection from later ones: a reader still running for an
    /// old connection must not act on a new one.
    id: u64,
    /// What the connection's writer is to do, in order.
    actions: mpsc::UnboundedSender<Action>,
}

impl Pair {
    /// Carries out what the model has due by `now`.
    fn flush(&mut self, now: Duration) {
        for action in self.pods.poll(now) {
            let Some(link) = &self.link else { break };
            let hang_up = action == Action::HangUp;
            let _ = link.actions.send(action);
            if hang_up {
                self.link = None;
            }
        }
    }
}

fn no_pair(mac: Address) -> anyhow::Error {
    anyhow::anyhow!("no pair at {mac}")
}

/// An adapter's power, for the logs.
fn power(powered: bool) -> &'static str {
    if powered { "powered on" } else { "powered off" }
}

impl World {
    /// A world on the bus at `bus`, its AAP sockets in `socket_dir`, which is
    /// created if missing, and its fake BlueZ with one adapter,
    /// [`DEFAULT_ADAPTER`], powered. No pair in it yet, and no name taken: add
    /// pairs, then [`World::publish`].
    ///
    /// Socket paths longer than 108 bytes cannot be bound, so `socket_dir`
    /// wants to be short.
    pub async fn new(bus: &str, socket_dir: &Path) -> Result<Self> {
        Self::with_adapters(bus, socket_dir, &[(DEFAULT_ADAPTER, true)]).await
    }

    /// [`World::new`], with `adapters` in place of the one: each a name, as
    /// `hci1`, and whether it is powered.
    pub async fn with_adapters(
        bus: &str,
        socket_dir: &Path,
        adapters: &[(&str, bool)],
    ) -> Result<Self> {
        let bluez = FakeBluez::serve(bus).await?;
        for &(name, powered) in adapters {
            bluez.add_adapter(name, powered).await?;
        }
        // Served on from the start, as `FakeWorld1` can be put there only
        // once the world it holds is made - see `connect_serving`.
        let control = connect_serving(bus).await?;
        std::fs::create_dir_all(socket_dir)
            .with_context(|| format!("failed to create {}", socket_dir.display()))?;
        let world = Self(Arc::new(Inner {
            bluez,
            control,
            bus: bus.to_string(),
            socket_dir: socket_dir.to_path_buf(),
            epoch: Instant::now(),
            pairs: Mutex::default(),
            due: Notify::new(),
            next_link: AtomicU64::new(0),
        }));
        let control = FakeWorld1 {
            world: world.clone(),
        };
        world.0.control.object_server().at(PATH, control).await?;
        tokio::spawn(world.clone().advertise());
        tokio::spawn(world.clone().keep_time());
        Ok(world)
    }

    /// Takes `org.bluez` and the world's own name. From here on the app can
    /// find it all, so whatever must be there when the app starts goes in
    /// first: a pair connected after the app started is one it is told about,
    /// one connected before is one it finds.
    pub async fn publish(&self) -> Result<()> {
        self.0.bluez.publish().await?;
        self.0
            .control
            .request_name(SERVICE)
            .await
            .with_context(|| format!("failed to take {SERVICE}"))
    }

    pub fn bluez(&self) -> &FakeBluez {
        &self.0.bluez
    }

    /// The models' time. Read with the pairs locked, so that no model sees
    /// time run backwards.
    fn now(&self) -> Duration {
        self.0.epoch.elapsed()
    }

    /// Pairs new AirPods with the host's first adapter, of those it has now:
    /// `product_id` as `parse_proximity_data` reads it, fresh keys, in range
    /// and disconnected.
    pub async fn add_pods(&self, mac: Address, product_id: u16, alias: &str) -> Result<()> {
        let adapters = self.0.bluez.adapters();
        let adapter = adapters.first().context("the world has no adapter")?;
        self.add_pods_on(adapter, mac, product_id, alias).await
    }

    /// [`World::add_pods`], with the adapter called `adapter`.
    pub async fn add_pods_on(
        &self,
        adapter: &str,
        mac: Address,
        product_id: u16,
        alias: &str,
    ) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        anyhow::ensure!(!pairs.contains_key(&mac), "{mac} is paired already");
        let path = unix_socket_path(&self.0.socket_dir, mac);
        // Left behind by a world that was killed; binding fails while it is.
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        let listener = UnixSeqpacketListener::bind(&path)
            .with_context(|| format!("failed to listen at {}", path.display()))?;
        self.0.bluez.add_device(adapter, mac, alias).await?;
        pairs.insert(
            mac,
            Pair {
                pods: Pods::new(product_id, mac),
                link: None,
                aap_attempts: 0,
            },
        );
        tokio::spawn(self.clone().accept(mac, listener));
        tracing::info!("fake world: {alias} paired at {mac} with {adapter}");
        Ok(())
    }

    /// The pairs' addresses, in order.
    pub async fn macs(&self) -> Vec<Address> {
        self.0.pairs.lock().await.keys().copied().collect()
    }

    /// Connects a pair to the host. The fake BlueZ signals it, and the app
    /// takes it up from there.
    pub async fn connect(&self, mac: Address) -> Result<()> {
        self.set_connected(mac, true).await
    }

    /// Disconnects a pair, and with it the AAP connection.
    pub async fn disconnect(&self, mac: Address) -> Result<()> {
        self.set_connected(mac, false).await
    }

    async fn set_connected(&self, mac: Address, connected: bool) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        if connected {
            // BlueZ first, as it turns away a pair whose adapter is off.
            self.0.bluez.set_connected(mac, true).await?;
            pair.pods.set_connected(true);
            pair.flush(self.now());
        } else {
            pair.pods.set_connected(false);
            // Disconnected, the model hangs up an open connection.
            pair.flush(self.now());
            self.0.bluez.set_connected(mac, false).await?;
        }
        tracing::info!(
            "fake world: {mac} {}",
            if connected {
                "connected"
            } else {
                "disconnected"
            }
        );
        Ok(())
    }

    /// Plugs in an adapter, as a USB dongle is - see
    /// [`FakeBluez::add_adapter`]. Pairs added from here on may pair with it.
    pub async fn add_adapter(&self, name: &str, powered: bool) -> Result<()> {
        self.0.bluez.add_adapter(name, powered).await?;
        tracing::info!("fake world: adapter {name} plugged in, {}", power(powered));
        Ok(())
    }

    /// Unplugs an adapter - see [`FakeBluez::remove_adapter`]. Its pairs
    /// lose their link, and AAP with it, and stay in range.
    pub async fn remove_adapter(&self, name: &str) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        self.drop_links(&mut pairs, name);
        self.0.bluez.remove_adapter(name).await?;
        tracing::info!("fake world: adapter {name} unplugged");
        Ok(())
    }

    /// Powers an adapter on or off - see [`FakeBluez::set_powered`]. Off, its
    /// pairs lose their link, and AAP with it; on again, they stay
    /// disconnected until connected.
    pub async fn set_powered(&self, name: &str, powered: bool) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        if !powered {
            self.drop_links(&mut pairs, name);
        }
        self.0.bluez.set_powered(name, powered).await?;
        tracing::debug!("fake world: adapter {name} {}", power(powered));
        Ok(())
    }

    /// Restarts the fake BlueZ on a connection of its own, and so under a
    /// new unique name - see [`FakeBluez::restart`]. The pairs keep
    /// their links, as the controllers keep them.
    pub async fn restart_bluez(&self) -> Result<()> {
        self.0.bluez.restart(&self.0.bus).await?;
        tracing::info!("fake world: BlueZ restarted");
        Ok(())
    }

    /// Disconnects the models of the pairs on the adapter called `adapter`,
    /// which hangs up their AAP connections. BlueZ is the caller's to tell.
    fn drop_links(&self, pairs: &mut BTreeMap<Address, Pair>, adapter: &str) {
        let now = self.now();
        for mac in self.0.bluez.paired_with(adapter) {
            if let Some(pair) = pairs.get_mut(&mac)
                && pair.pods.is_connected()
            {
                pair.pods.set_connected(false);
                pair.flush(now);
            }
        }
    }

    /// Closes a pair's AAP connection from its end. The Bluetooth link stays.
    pub async fn hang_up(&self, mac: Address) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        let link = pair
            .link
            .take()
            .with_context(|| format!("{mac} has no AAP connection"))?;
        pair.pods.aap_closed();
        let _ = link.actions.send(Action::HangUp);
        tracing::info!("fake world: {mac} hung up AAP");
        Ok(())
    }

    /// Changes a pair through its model - [`Pods::set_battery`],
    /// [`Pods::repair`], [`Pods::quirks`] and the rest - and carries out what
    /// the change brings about over AAP. Returns what `change` does, so it
    /// reads the model too.
    ///
    /// Two parts of the model are the world's own: connecting, which goes
    /// through [`World::connect`] and [`World::disconnect`] to tell the fake
    /// BlueZ as well, and the AAP side - `aap_opened`, `receive`, `poll`.
    pub async fn with_pods<R>(
        &self,
        mac: Address,
        change: impl FnOnce(&mut Pods) -> R,
    ) -> Result<R> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        let result = change(&mut pair.pods);
        pair.flush(self.now());
        self.0.due.notify_one();
        Ok(result)
    }

    /// A pair's keys, for a test to hand the app's keystore.
    pub async fn keys(&self, mac: Address) -> Result<Keys> {
        self.with_pods(mac, |pods| pods.keys().clone()).await
    }

    /// Every AAP packet the app has sent a pair, in order.
    pub async fn received(&self, mac: Address) -> Result<Vec<Vec<u8>>> {
        self.with_pods(mac, |pods| pods.received().to_vec()).await
    }

    /// How many AAP connections the app has opened to a pair, those turned
    /// away included.
    pub async fn aap_attempts(&self, mac: Address) -> Result<usize> {
        let pairs = self.0.pairs.lock().await;
        let pair = pairs.get(&mac).ok_or_else(|| no_pair(mac))?;
        Ok(pair.aap_attempts)
    }

    /// What GNOME Settings shows: each device's level, by MAC.
    pub fn batteries(&self) -> HashMap<Address, u8> {
        self.0.bluez.batteries()
    }

    /// `count` advertisers no key decrypts, as a BLE spam flood brings: each a
    /// pair with keys nobody holds, so its advertisement parses but stays
    /// closed, advertising once, from an address of its own. A new address is
    /// signaled only as it appears, with `InterfacesAdded`: the scanner reads
    /// it, attachment's connection watch does not. Gone after the fake
    /// BlueZ's timeout.
    pub async fn flood(&self, count: u32) -> Result<()> {
        self.spam(count, 1).await
    }

    /// `count` advertisers no key decrypts, as [`World::flood`] brings, each
    /// advertising `times` times from one address. Every advertisement after
    /// an advertiser's first is signaled as a change, with
    /// `PropertiesChanged`, which attachment's connection watch subscribes to
    /// as well as the scanner.
    pub async fn flood_repeating(&self, count: u32, times: u32) -> Result<()> {
        self.spam(count, times).await
    }

    async fn spam(&self, count: u32, times: u32) -> Result<()> {
        let now = self.now();
        for _ in 0..count {
            let spammer = Pods::new(PRO_3, random_address());
            let address = spammer.address_at(now);
            for i in 0..times {
                let advert = spammer.advertisement(now + Duration::from_millis(i.into()));
                self.0.bluez.advertise(address, &advert.data).await?;
            }
        }
        tracing::debug!("fake world: flooded with {count} advertisers, {times} time(s) each");
        Ok(())
    }

    /// Advertises every pair every [`ADVERTISING_INTERVAL`], and has the fake
    /// BlueZ forget the addresses that went quiet.
    async fn advertise(self) {
        let mut tick = tokio::time::interval(ADVERTISING_INTERVAL);
        loop {
            tick.tick().await;
            let adverts: Vec<_> = {
                let pairs = self.0.pairs.lock().await;
                let now = self.now();
                pairs
                    .iter()
                    .map(|(mac, pair)| (*mac, pair.pods.advertisement(now)))
                    .collect()
            };
            for (mac, advert) in adverts {
                match self.0.bluez.advertise(advert.address, &advert.data).await {
                    Ok(true) => {
                        tracing::trace!("fake world: {mac} advertised as {}", advert.address)
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!("fake world: {mac} failed to advertise: {e:#}"),
                }
            }
            self.0.bluez.prune(Instant::now()).await;
        }
    }

    /// Polls the pairs when one has something scheduled, as
    /// [`Pods::next_due`] asks.
    async fn keep_time(self) {
        loop {
            let next = {
                let pairs = self.0.pairs.lock().await;
                pairs.values().filter_map(|p| p.pods.next_due()).min()
            };
            // A change since `next` was read left a permit, so this returns
            // at once and `next` is read again.
            let changed = self.0.due.notified();
            match next {
                Some(at) => {
                    let at = tokio::time::Instant::from_std(self.0.epoch + at);
                    tokio::select! {
                        _ = tokio::time::sleep_until(at) => {}
                        _ = changed => continue,
                    }
                }
                None => {
                    changed.await;
                    continue;
                }
            }
            let mut pairs = self.0.pairs.lock().await;
            let now = self.now();
            for pair in pairs.values_mut() {
                pair.flush(now);
            }
        }
    }

    /// Takes the app's AAP connections to one pair.
    async fn accept(self, mac: Address, mut listener: UnixSeqpacketListener) {
        loop {
            match listener.accept().await {
                Ok(socket) => self.link_opened(mac, socket).await,
                Err(e) => {
                    tracing::warn!("fake world: stopped taking AAP connections to {mac}: {e}");
                    return;
                }
            }
        }
    }

    async fn link_opened(&self, mac: Address, socket: UnixSeqpacket) {
        let mut pairs = self.0.pairs.lock().await;
        let Some(pair) = pairs.get_mut(&mac) else {
            return;
        };
        pair.aap_attempts += 1;
        if !self.0.bluez.can_connect(mac) {
            // There is no L2CAP without the adapter.
            tracing::info!("fake world: turned away AAP to {mac}: its adapter is off or gone");
            return;
        }
        if pair.link.is_some() {
            // What AirPods do with a second connection is not recorded.
            // Turning it away keeps the first, the one the app uses: a
            // second only comes from a connect racing another.
            tracing::info!("fake world: turned away a second AAP connection to {mac}");
            return;
        }
        if pair.pods.refuses_aap() {
            tracing::info!("fake world: {mac} turned AAP away, as its quirk asks");
            return;
        }
        let now = self.now();
        let was_connected = pair.pods.is_connected();
        pair.pods.aap_opened(now);
        let socket = Arc::new(socket);
        let (actions, queue) = mpsc::unbounded_channel();
        let id = self.0.next_link.fetch_add(1, Ordering::Relaxed);
        pair.link = Some(Link { id, actions });
        tokio::spawn(write_link(socket.clone(), queue));
        tokio::spawn(self.clone().read_link(mac, id, socket));
        pair.flush(now);
        self.0.due.notify_one();
        tracing::info!("fake world: AAP connected to {mac}");

        // There is no AAP without a Bluetooth link, so the connection brought
        // one up, as an L2CAP connect does.
        if !was_connected && let Err(e) = self.0.bluez.set_connected(mac, true).await {
            tracing::warn!("fake world: failed to mark {mac} connected: {e:#}");
        }
    }

    /// Hands the model what the app sends on one connection, until it closes.
    async fn read_link(self, mac: Address, id: u64, socket: Arc<UnixSeqpacket>) {
        let mut buf = vec![0u8; READ_BUF_LEN];
        loop {
            // A closed connection reads as zero bytes, as L2CAP's does.
            let read = socket.recv(&mut buf).await.map(|info| info.bytes_read());
            let mut pairs = self.0.pairs.lock().await;
            let Some(pair) = pairs
                .get_mut(&mac)
                .filter(|p| p.link.as_ref().is_some_and(|l| l.id == id))
            else {
                // Hung up from this end, which closed it.
                return;
            };
            let now = self.now();
            match read {
                Ok(n) if n > 0 => {
                    pair.pods.receive(&buf[..n], now);
                    pair.flush(now);
                    self.0.due.notify_one();
                }
                _ => {
                    pair.pods.aap_closed();
                    pair.link = None;
                    tracing::info!("fake world: the app closed AAP to {mac}");
                    return;
                }
            }
        }
    }
}

/// Sends what the model sends, until it hangs up or the link is dropped -
/// either way closing the connection, which the app reads as such.
async fn write_link(socket: Arc<UnixSeqpacket>, mut queue: mpsc::UnboundedReceiver<Action>) {
    while let Some(Action::Send(packet)) = queue.recv().await {
        if socket.send(&packet).await.is_err() {
            break;
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
}

/// An address for a flood's advertiser, which only its suffix in the payload
/// ever shows.
fn random_address() -> Address {
    let bytes = random_key();
    Address::new([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]])
}

fn parse_mac(mac: &str) -> fdo::Result<Address> {
    mac.parse()
        .map_err(|_| fdo::Error::InvalidArgs(format!("not a MAC: {mac:?}")))
}

fn parse_part(part: &str) -> Result<Part, String> {
    match part {
        "left" => Ok(Part::Left),
        "right" => Ok(Part::Right),
        "case" => Ok(Part::Case),
        _ => Err(format!("no part {part:?}: left, right or case")),
    }
}

/// Sets the quirk named by its field in [`Quirks`]. `echo_delay`,
/// `report_delay` and `stale_report` go by `echo_delay_ms`, `report_delay_ms`
/// and `stale_report_ms`, in milliseconds, a `stale_report_ms` of 0 turning
/// the quirk off. A number may come as any integer type,
/// as `gdbus` sends `<4>` as an `int32`.
fn set_quirk(quirks: &mut Quirks, name: &str, value: &Value<'_>) -> Result<(), String> {
    let flag = || bool::try_from(value).map_err(|_| format!("{name} takes a boolean"));
    let count = || {
        let n = match value {
            Value::U8(n) => i64::from(*n),
            Value::U16(n) => i64::from(*n),
            Value::I16(n) => i64::from(*n),
            Value::U32(n) => i64::from(*n),
            Value::I32(n) => i64::from(*n),
            Value::I64(n) => *n,
            Value::U64(n) => i64::try_from(*n).unwrap_or(-1),
            _ => -1,
        };
        u32::try_from(n).map_err(|_| format!("{name} takes a number from 0 to {}", u32::MAX))
    };
    match name {
        "off_needs_allow" => quirks.off_needs_allow = flag()?,
        "settings_changed" => quirks.settings_changed = flag()?,
        "echo_every" => quirks.echo_every = count()?,
        "echo_delay_ms" => quirks.echo_delay = Duration::from_millis(count()?.into()),
        "zero_suffix_when_connected" => quirks.zero_suffix_when_connected = flag()?,
        "silent_handshake" => quirks.silent_handshake = flag()?,
        "refuse_aap" => quirks.refuse_aap = count()?,
        "report_delay_ms" => quirks.report_delay = Duration::from_millis(count()?.into()),
        "stale_report_ms" => {
            quirks.stale_report =
                Some(Duration::from_millis(count()?.into())).filter(|delay| !delay.is_zero());
        }
        _ => return Err(format!("no quirk {name:?}")),
    }
    Ok(())
}

fn failed(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

/// The world's control interface - see the module docs.
struct FakeWorld1 {
    world: World,
}

#[interface(name = "io.github.mstroecker.LinuxPods.FakeWorld1")]
impl FakeWorld1 {
    async fn add_pods(&self, mac: &str, model: u16, alias: &str) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world.add_pods(mac, model, alias).await.map_err(failed)
    }

    async fn add_pods_on(
        &self,
        adapter: &str,
        mac: &str,
        model: u16,
        alias: &str,
    ) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .add_pods_on(adapter, mac, model, alias)
            .await
            .map_err(failed)
    }

    async fn add_adapter(&self, name: &str, powered: bool) -> fdo::Result<()> {
        self.world.add_adapter(name, powered).await.map_err(failed)
    }

    async fn remove_adapter(&self, name: &str) -> fdo::Result<()> {
        self.world.remove_adapter(name).await.map_err(failed)
    }

    async fn set_powered(&self, name: &str, powered: bool) -> fdo::Result<()> {
        self.world.set_powered(name, powered).await.map_err(failed)
    }

    async fn restart_bluez(&self) -> fdo::Result<()> {
        self.world.restart_bluez().await.map_err(failed)
    }

    async fn connect(&self, mac: &str) -> fdo::Result<()> {
        self.world.connect(parse_mac(mac)?).await.map_err(failed)
    }

    async fn disconnect(&self, mac: &str) -> fdo::Result<()> {
        self.world.disconnect(parse_mac(mac)?).await.map_err(failed)
    }

    async fn set_battery(&self, mac: &str, part: &str, level: u8) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        let part = parse_part(part).map_err(fdo::Error::InvalidArgs)?;
        let level = (level <= 100).then_some(level);
        self.world
            .with_pods(mac, |pods| pods.set_battery(part, level))
            .await
            .map_err(failed)
    }

    async fn set_lid(&self, mac: &str, open: bool) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .with_pods(mac, |pods| pods.set_lid(open))
            .await
            .map_err(failed)
    }

    async fn repair(&self, mac: &str) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .with_pods(mac, Pods::repair)
            .await
            .map_err(failed)
    }

    async fn hang_up(&self, mac: &str) -> fdo::Result<()> {
        self.world.hang_up(parse_mac(mac)?).await.map_err(failed)
    }

    async fn set_quirk(&self, mac: &str, quirk: &str, value: OwnedValue) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .with_pods(mac, |pods| set_quirk(&mut pods.quirks, quirk, &value))
            .await
            .map_err(failed)?
            .map_err(fdo::Error::InvalidArgs)
    }

    async fn flood(&self, count: u32) -> fdo::Result<()> {
        self.world.flood(count).await.map_err(failed)
    }

    async fn received(&self, mac: &str) -> fdo::Result<Vec<Vec<u8>>> {
        self.world.received(parse_mac(mac)?).await.map_err(failed)
    }

    fn batteries(&self) -> HashMap<String, u8> {
        self.world
            .batteries()
            .into_iter()
            .map(|(mac, level)| (mac.to_string(), level))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use futures_util::StreamExt;
    use tokio::time::timeout;
    use zbus::proxy::CacheProperties;
    use zbus::{MatchRule, MessageStream};

    use super::*;
    use crate::aap::noise::{self, NoiseMode};
    use crate::aap::{Client, parse_battery_packet};
    use crate::ble::decrypt::decrypt_for_device;
    use crate::ble::scanner::Scanner;
    use crate::bluez::BLUEZ_SERVICE;
    use crate::harness::PrivateBus;
    use crate::harness::bluez::device_path;
    use crate::harness::pods::PRO_2_USB_C;

    /// Plenty for any of these, and a hang fails the test instead of CI.
    /// Advertisements take longest: up to an interval before the app hears
    /// one.
    const TIMEOUT: Duration = Duration::from_secs(5);

    const MAC: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

    /// A published world on a bus of its own with one pair in it, and the
    /// directory its sockets are in.
    async fn world() -> (PrivateBus, World, tempfile::TempDir) {
        let bus = PrivateBus::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let world = World::new(bus.address(), dir.path()).await.unwrap();
        world.add_pods(MAC, PRO_3, "Fake AirPods").await.unwrap();
        world.publish().await.unwrap();
        (bus, world, dir)
    }

    /// An AAP client connected to the pair at `mac`, as the coordinator opens
    /// one under the harness.
    async fn aap(dir: &tempfile::TempDir, mac: Address) -> Client {
        let mut client = Client::with_unix_socket(mac, unix_socket_path(dir.path(), mac));
        client.connect().await.unwrap();
        client
    }

    async fn next_packet(client: &Client) -> anyhow::Result<Vec<u8>> {
        timeout(TIMEOUT, client.read_packet()).await.unwrap()
    }

    /// Whether the fake BlueZ shows the pair connected, read afresh.
    async fn bluez_connected(bus: &PrivateBus, mac: Address) -> bool {
        let conn = bus.connect().await.unwrap();
        let path = device_path(DEFAULT_ADAPTER, mac);
        let device = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination(BLUEZ_SERVICE)
            .unwrap()
            .path(path.as_str())
            .unwrap()
            .interface("org.bluez.Device1")
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap();
        device.get_property("Connected").await.unwrap()
    }

    #[test]
    fn parts_and_quirks_go_by_name() {
        assert_eq!(parse_part("left"), Ok(Part::Left));
        assert_eq!(parse_part("case"), Ok(Part::Case));
        assert!(parse_part("Left").is_err());

        let mut quirks = Quirks::default();
        set_quirk(&mut quirks, "silent_handshake", &Value::from(true)).unwrap();
        set_quirk(&mut quirks, "echo_every", &Value::from(1i32)).unwrap();
        set_quirk(&mut quirks, "echo_delay_ms", &Value::from(20u32)).unwrap();
        set_quirk(&mut quirks, "refuse_aap", &Value::from(3i32)).unwrap();
        assert!(quirks.silent_handshake);
        assert_eq!(quirks.refuse_aap, 3);
        assert_eq!(quirks.echo_every, 1);
        assert_eq!(quirks.echo_delay, Duration::from_millis(20));
        set_quirk(&mut quirks, "stale_report_ms", &Value::from(330u32)).unwrap();
        assert_eq!(quirks.stale_report, Some(Duration::from_millis(330)));
        set_quirk(&mut quirks, "stale_report_ms", &Value::from(0u32)).unwrap();
        assert_eq!(quirks.stale_report, None);

        assert!(set_quirk(&mut quirks, "echo_every", &Value::from(-1i32)).is_err());
        assert!(set_quirk(&mut quirks, "echo_every", &Value::from(true)).is_err());
        assert!(set_quirk(&mut quirks, "silent_handshake", &Value::from(1u32)).is_err());
        assert!(set_quirk(&mut quirks, "echo_delay", &Value::from(1u32)).is_err());
    }

    /// A pair answers on its socket, tells of a change, records what it was
    /// sent, and its hanging up reaches the app as a closed connection. The
    /// connection brought the Bluetooth link up with it.
    #[tokio::test]
    async fn a_pair_answers_on_its_socket() {
        let (bus, world, dir) = world().await;
        let client = aap(&dir, MAC).await;

        // Back once the pair has answered it.
        client.handshake().await.unwrap();
        client.request_battery_status().await.unwrap();
        let info = parse_battery_packet(&next_packet(&client).await.unwrap()).unwrap();
        assert_eq!(info.left.unwrap().level, Some(80));
        assert!(bluez_connected(&bus, MAC).await);

        // The rest of the startup dump, then the change.
        world
            .with_pods(MAC, |pods| pods.set_battery(Part::Left, Some(42)))
            .await
            .unwrap();
        let changed = loop {
            let packet = next_packet(&client).await.unwrap();
            if let Ok(info) = parse_battery_packet(&packet) {
                break info;
            }
        };
        assert_eq!(changed.left.unwrap().level, Some(42));
        assert_eq!(world.received(MAC).await.unwrap().len(), 2);

        world.hang_up(MAC).await.unwrap();
        let closed = loop {
            if let Err(e) = next_packet(&client).await {
                break e;
            }
        };
        assert_eq!(closed.to_string(), "connection closed");
        assert!(!world.with_pods(MAC, |pods| pods.has_aap()).await.unwrap());
        assert!(bluez_connected(&bus, MAC).await, "the link stays");
        assert!(world.hang_up(MAC).await.is_err(), "nothing left to hang up");
    }

    /// Disconnecting takes the AAP connection along, and BlueZ says so.
    #[tokio::test]
    async fn disconnecting_closes_the_connection() {
        let (bus, world, dir) = world().await;
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        client.handshake().await.unwrap();

        world.disconnect(MAC).await.unwrap();
        assert!(next_packet(&client).await.is_err());
        assert!(!bluez_connected(&bus, MAC).await);
    }

    /// An adapter powering off takes its pairs' links, and AAP with them, and
    /// nothing connects until it is on again. Unplugged, it takes them too.
    #[tokio::test]
    async fn an_adapter_going_takes_its_pairs_links() {
        let (bus, world, dir) = world().await;
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        client.handshake().await.unwrap();

        world.set_powered(DEFAULT_ADAPTER, false).await.unwrap();
        assert!(next_packet(&client).await.is_err(), "AAP stays up");
        assert!(!bluez_connected(&bus, MAC).await);
        assert!(
            !world
                .with_pods(MAC, |pods| pods.is_connected())
                .await
                .unwrap()
        );
        assert!(world.connect(MAC).await.is_err(), "connected while off");
        let turned_away = aap(&dir, MAC).await;
        assert!(next_packet(&turned_away).await.is_err());

        world.set_powered(DEFAULT_ADAPTER, true).await.unwrap();
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        client.handshake().await.unwrap();

        world.remove_adapter(DEFAULT_ADAPTER).await.unwrap();
        assert!(next_packet(&client).await.is_err(), "AAP stays up");
        assert!(
            !world
                .with_pods(MAC, |pods| pods.is_connected())
                .await
                .unwrap()
        );
        assert!(
            world.connect(MAC).await.is_err(),
            "connected with no adapter"
        );
    }

    /// A second connection is turned away, and the first carries on.
    #[tokio::test]
    async fn a_second_connection_is_turned_away() {
        let (_bus, _world, dir) = world().await;
        let first = aap(&dir, MAC).await;
        first.handshake().await.unwrap();

        let second = aap(&dir, MAC).await;
        assert!(next_packet(&second).await.is_err());
        first.request_battery_status().await.unwrap();
        assert!(next_packet(&first).await.is_ok());
    }

    /// What the model schedules comes when it is due: here the echo of a mode
    /// change, after its delay.
    #[tokio::test]
    async fn timed_replies_come_when_due() {
        let (_bus, world, dir) = world().await;
        world
            .with_pods(MAC, |pods| {
                pods.quirks.echo_every = 1;
                pods.quirks.echo_delay = Duration::from_millis(50);
            })
            .await
            .unwrap();
        let client = aap(&dir, MAC).await;
        client.handshake().await.unwrap();

        client.set_noise_mode(NoiseMode::Adaptive).await.unwrap();
        assert_eq!(
            next_packet(&client).await.unwrap(),
            noise::settings_changed_packet()
        );
        assert_eq!(
            next_packet(&client).await.unwrap(),
            noise::set_packet(NoiseMode::Adaptive)
        );
    }

    /// Every pair advertises while the app discovers, and its key decrypts
    /// what it sends.
    #[tokio::test]
    async fn every_pair_advertises() {
        let (bus, world, _dir) = world().await;
        world.add_pods(OTHER, PRO_2_USB_C, "Other").await.unwrap();
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        scanner.start_discovery().await.unwrap();

        let keys = [
            (MAC, world.keys(MAC).await.unwrap()),
            (OTHER, world.keys(OTHER).await.unwrap()),
        ];
        let mut heard = HashSet::new();
        timeout(TIMEOUT, async {
            while heard.len() < keys.len() {
                let advert = adverts.next().await.unwrap();
                let encrypted = advert.data.encrypted_portion().unwrap();
                for (mac, keys) in &keys {
                    if decrypt_for_device(encrypted, &keys.enc_key, *mac).is_ok() {
                        heard.insert(*mac);
                    }
                }
            }
        })
        .await
        .expect("heard both pairs");
    }

    /// A flood's advertisers parse, come each from an address of its own, and
    /// the pair's key decrypts none of them.
    #[tokio::test]
    async fn a_flood_decrypts_with_no_key() {
        let (bus, world, _dir) = world().await;
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        scanner.start_discovery().await.unwrap();
        let key = world.keys(MAC).await.unwrap().enc_key;

        world.flood(3).await.unwrap();
        let mut addresses = HashSet::new();
        // By address, so that three are three addresses of their own.
        timeout(TIMEOUT, async {
            while addresses.len() < 3 {
                let advert = adverts.next().await.unwrap();
                let encrypted = advert.data.encrypted_portion().unwrap();
                if decrypt_for_device(encrypted, &key, MAC).is_err() {
                    addresses.insert(advert.ble_mac);
                }
            }
        })
        .await
        .expect("heard the flood, from three addresses");
    }

    /// A flood is signaled only as its addresses appear, and a repeating
    /// flood as changes after that: what attachment's connection watch
    /// subscribes to, and the #90 test relies on. A world with no pair in it,
    /// so that every device signaled is an advertiser's.
    #[tokio::test]
    async fn only_a_repeating_flood_is_signaled_as_changes() {
        let bus = PrivateBus::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let world = World::new(bus.address(), dir.path()).await.unwrap();
        world.publish().await.unwrap();
        let conn = bus.connect().await.unwrap();
        // The connection watch's rule.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .unwrap()
            .member("PropertiesChanged")
            .unwrap()
            .path_namespace("/org/bluez")
            .unwrap()
            .build();
        let changes = MessageStream::for_match_rule(rule, &conn, None)
            .await
            .unwrap()
            .filter_map(|msg| async move {
                let msg = msg.ok()?;
                let (iface, _, _) = msg
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    .ok()?;
                let path = msg.header().path()?.to_string();
                (iface == "org.bluez.Device1").then_some(path)
            });
        tokio::pin!(changes);
        Scanner::with_connection(conn)
            .start_discovery()
            .await
            .unwrap();

        // Signaled in order, so a change from the plain flood would come
        // among the first four.
        world.flood(3).await.unwrap();
        world.flood_repeating(2, 3).await.unwrap();
        let mut changed: HashMap<String, usize> = HashMap::new();
        timeout(TIMEOUT, async {
            for _ in 0..4 {
                *changed.entry(changes.next().await.unwrap()).or_default() += 1;
            }
        })
        .await
        .expect("heard four changes");
        assert_eq!(
            changed.values().copied().collect::<Vec<_>>(),
            [2, 2],
            "by device: {changed:?}"
        );
    }

    /// `FakeWorld1` reaches the world, and turns away what it cannot take.
    #[tokio::test]
    async fn fake_world1_changes_the_world() {
        let (bus, world, _dir) = world().await;
        let conn = bus.connect().await.unwrap();
        let control = zbus::Proxy::new(&conn, SERVICE, PATH, INTERFACE)
            .await
            .unwrap();
        let other = OTHER.to_string();

        control
            .call_method("AddPods", &(&other, PRO_2_USB_C, "Other"))
            .await
            .unwrap();
        assert_eq!(world.macs().await, [MAC, OTHER]);
        let keys = world.keys(OTHER).await.unwrap();

        control
            .call_method("SetBattery", &(&other, "left", 42u8))
            .await
            .unwrap();
        control
            .call_method("SetBattery", &(&other, "case", 255u8))
            .await
            .unwrap();
        control
            .call_method("SetQuirk", &(&other, "echo_every", Value::from(2i32)))
            .await
            .unwrap();
        control.call_method("Repair", &(&other,)).await.unwrap();
        let (left, case, echo_every, repaired) = world
            .with_pods(OTHER, |pods| {
                (
                    pods.battery(Part::Left),
                    pods.battery(Part::Case),
                    pods.quirks.echo_every,
                    pods.keys() != &keys,
                )
            })
            .await
            .unwrap();
        assert_eq!((left, case, echo_every), (Some(42), None, 2));
        assert!(repaired);

        control.call_method("Connect", &(&other,)).await.unwrap();
        assert!(bluez_connected(&bus, OTHER).await);
        control.call_method("Disconnect", &(&other,)).await.unwrap();
        assert!(!bluez_connected(&bus, OTHER).await);

        let received: Vec<Vec<u8>> = control.call("Received", &(&other,)).await.unwrap();
        assert!(received.is_empty());
        let batteries: HashMap<String, u8> = control.call("Batteries", &()).await.unwrap();
        assert!(batteries.is_empty());
        control.call_method("Flood", &(2u32,)).await.unwrap();

        // Adapters, and the fake BlueZ behind them.
        let third = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x03]).to_string();
        control
            .call_method("AddAdapter", &("hci1", false))
            .await
            .unwrap();
        control
            .call_method("AddPodsOn", &("hci1", &third, PRO_3, "Third"))
            .await
            .unwrap();
        assert!(
            control.call_method("Connect", &(&third,)).await.is_err(),
            "while off"
        );
        control
            .call_method("SetPowered", &("hci1", true))
            .await
            .unwrap();
        control.call_method("Connect", &(&third,)).await.unwrap();
        control
            .call_method("RemoveAdapter", &("hci1",))
            .await
            .unwrap();
        assert_eq!(world.bluez().adapters(), [DEFAULT_ADAPTER]);
        control.call_method("RestartBluez", &()).await.unwrap();
        assert!(world.bluez().is_powered(DEFAULT_ADAPTER));

        for (method, error) in [
            (
                control
                    .call_method("SetBattery", &(&other, "middle", 1u8))
                    .await,
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            (
                control.call_method("Connect", &("not a mac",)).await,
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            (
                control
                    .call_method("SetQuirk", &(&other, "loud", Value::from(true)))
                    .await,
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            (
                control.call_method("HangUp", &(&other,)).await,
                "org.freedesktop.DBus.Error.Failed",
            ),
            (
                control
                    .call_method("AddPods", &(&other, PRO_3, "Again"))
                    .await,
                "org.freedesktop.DBus.Error.Failed",
            ),
        ] {
            match method {
                Err(zbus::Error::MethodError(name, _, _)) => assert_eq!(name.as_str(), error),
                other => panic!("expected {error}, got {other:?}"),
            }
        }
    }
}
