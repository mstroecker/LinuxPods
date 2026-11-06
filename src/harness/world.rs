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
//! - **BLE.** Every [`ADVERTISING_INTERVAL`] the advertisement of each pod
//!   that advertises goes to the fake BlueZ, which signals it while the app
//!   discovers - from the resolvable private address the model makes for the
//!   pod from the pair's IRK and rotates, so each pod brings a new device
//!   object every ten seconds. Both pods advertise while one is in the case,
//!   only the primary while both are out, and none from the closed case -
//!   see [`Pods::advertisements`]. A pod repeats its payload until something
//!   changes, so most of what the fake BlueZ signals is the strength alone,
//!   as with real AirPods. Every adapter that discovers hears it, whichever
//!   the pair is paired with, as radios in one room do, so with two adapters
//!   each advertisement is signaled twice, from a device under each. When
//!   the last client stops discovering, the pods' devices go, as
//!   bluetoothd's do, and the next to discover hears each pod afresh at its
//!   next advertisement - see [`FakeBluez`].
//! - **AAP.** Each pair listens at `aap::client::unix_socket_path(dir, mac)`,
//!   where the app connects in place of L2CAP. What the app sends goes to
//!   [`Pods::receive`], and [`Pods::poll`] runs after that and after every
//!   change, and again when [`Pods::next_due`] says, for what comes late:
//!   the handshake's answer, the startup dump, the echo of a mode change, a
//!   stretch of speech, the silent pair's hang-up. A pair that drops its link, as with both pods in
//!   the closed case, is disconnected in the fake BlueZ, then its AAP
//!   connection closed, as a disconnect is - see [`World::disconnect`].
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
//! | `AddPods(s mac, q model, s name)` | Pairs new AirPods with the world's first adapter - model by product ID, such as `0x2027` for Pro 3 - in range and disconnected, named `name`, with no alias set |
//! | `AddPodsOn(s adapter, s mac, q model, s name)` | The same, with the adapter called `adapter` |
//! | `AddAdapter(s name, b powered)` | Plugs in an adapter at `/org/bluez/<name>`; one plugged in before has its pairs back, disconnected |
//! | `RemoveAdapter(s name)` | Unplugs it: its pairs disconnect, and its devices and battery providers go |
//! | `SetPowered(s name, b powered)` | Powers it on or off; off, discovery there ends and its pairs disconnect |
//! | `RestartBluez()` | bluetoothd restarting as after a crash, under a new unique name: `NameOwnerChanged` twice, and every client's discovery and battery providers forgotten. Nothing is announced again, as a real one would - see [`FakeBluez::restart`] |
//! | `Connect(s mac)` | Connects them to the host, which the app takes up; their services are resolved 4 s later, as bluetoothd signals - see [`FakeBluez::set_connected`] |
//! | `Disconnect(s mac)` | Disconnects them, which takes the AAP connection along, after BlueZ's `Connected=false` |
//! | `SetBattery(s mac, s part, y level)` | `left`, `right` or `case`; a level above 100 is unknown |
//! | `SetInEar(s mac, s pod, b in_ear)` | `left` or `right` into an ear, out of the case, or out of the ear; over AAP an ear report follows - see [`Pods::set_in_ear`] |
//! | `SetInCase(s mac, s pod, b in_case)` | `left` or `right` into the case, out of the ear, or out of the case - see [`Pods::set_in_case`] |
//! | `SetLid(s mac, b open)` | Opens or closes the case; closed on both pods, it drops their link. The pods in the closed case stop advertising 4 s later, and come back under new addresses once it opens, the right pod 1 s later, the left 5.5 s |
//! | `Repair(s mac)` | Re-pairs: new keys, sent at the next key request |
//! | `HangUp(s mac)` | Closes the AAP connection from the AirPods' end; the Bluetooth link stays |
//! | `SetAlias(s mac, s alias)` | Sets the pair's BlueZ alias, as GNOME Settings' rename does; `""` clears it, and the alias is the name again |
//! | `Name(s mac) → s` | The pods' own name, as they hold it: what a rename set, which the next AAP connection's startup dump carries, and BlueZ's `Name` once they connect again. Not UTF-8 where a rename cut a character, which shows as U+FFFD |
//! | `SetConversationAwareness(s mac, b on)` | Turns Conversation Awareness on or off, as from a phone: nothing reports it over AAP, the next connection's startup dump does |
//! | `Speak(s mac, u length_ms)` | The user speaks for that long. With Conversation Awareness on, the pods send speech levels and switch to Transparency and back, at the #150 run's times - see [`Pods::speak`]; with it off, nothing. Fails with no AAP connection |
//! | `SetPlaying(s mac, b playing)` | Audio to a connected pair starts or stops, as the audio server's: its A2DP transport `active`, set up at a new path if it has none, at the pair's own volume; or `idle` - see [`FakeBluez::set_playing`] |
//! | `SetCall(s mac, b on)` | A call begins or ends on a connected pair: A2DP `idle`, and back after - see [`FakeBluez::set_call`] |
//! | `SetVolume(s mac, q volume)` | The user sets the pair's A2DP volume, 0 to 127: GNOME's slider, say; with no transport, the volume the next starts at |
//! | `SetVolumeAnswer(s mac, u delay_ms, b to_even)` | The pair answers a volume the app sets that long later, rounded down to even if asked, as bluetoothd has real pods answer - see [`FakeBluez::set_volume_answer`] |
//! | `Volume(s mac) → q` | The volume of the pair's A2DP transport; fails without one |
//! | `VolumeSets(s mac) → aq` | Every volume the app set on the pair's transports, in order |
//! | `SetQuirk(s mac, s quirk, v value)` | Sets one of [`Quirks`] by its field name, the durations by theirs with `_ms` - `echo_delay_ms`, `handshake_delay_ms`, `report_delay_ms`, `dump_delay_ms`, `stale_report_ms` - in milliseconds, a `stale_report_ms` of 0 for none |
//! | `Flood(u count)` | That many advertisers no key decrypts and no IRK resolves, each from an address of its own |
//! | `Received(s mac) → aay` | Every AAP packet the app sent the pair, in order |
//! | `Batteries() → a{sy}` | What GNOME Settings shows: the level a battery provider gave each device, by MAC |
//!
//! A malformed argument is `org.freedesktop.DBus.Error.InvalidArgs`, and a
//! request the world cannot carry out - a MAC it has no pair at, a hang-up
//! with no connection - is `org.freedesktop.DBus.Error.Failed`.
//!
//! Time is real: each pod rotates its address every ten seconds - far more
//! often than real AirPods, every 4 to 8 minutes, so that tests meet rotation -
//! and the fake BlueZ forgets an address after thirty, or as soon as nobody
//! discovers. The model's clock starts with the world.

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

use super::bluez::{FakeBluez, VolumeAnswer};
use super::connect_serving;
use super::pods::{Action, Keys, PRO_3, Part, Pods, Quirks};
use crate::aap::client::unix_socket_path;
use crate::ble::decrypt::random_key;
use crate::ble::parser::PodSide;

/// The name `FakeWorld1` is served under.
pub const SERVICE: &str = "io.github.mstroecker.LinuxPods.FakeWorld";
pub const PATH: &str = "/io/github/mstroecker/LinuxPods/FakeWorld";
pub const INTERFACE: &str = "io.github.mstroecker.LinuxPods.FakeWorld1";

/// The adapter a world has unless it is made with others: the one bluetoothd
/// puts first.
pub const DEFAULT_ADAPTER: &str = "hci0";

/// How often each pod advertises. Real AirPods do so about 5 times a second
/// from each pod, with bursts of 30-35 a second after the lid opens or the
/// pods go back in the case (#71). Once a second keeps the logs readable, and
/// still gives an address ten advertisements before it rotates.
pub const ADVERTISING_INTERVAL: Duration = Duration::from_secs(1);

/// Room for any AAP packet the app sends, a rename (`aap::name`) among
/// them.
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
    /// Hands `action` to the AAP connection's writer, if there is one. A
    /// hang-up ends the connection.
    fn carry_out(&mut self, action: Action) {
        let Some(link) = &self.link else { return };
        let hang_up = action == Action::HangUp;
        let _ = link.actions.send(action);
        if hang_up {
            self.link = None;
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

    /// The models' time: what [`Pods`] takes as `now`, counted from the
    /// world's start. The world reads it for a pair's model with the pairs
    /// locked, so that no model sees time run backwards.
    pub fn now(&self) -> Duration {
        self.0.epoch.elapsed()
    }

    /// Pairs new AirPods with the host's first adapter, of those it has now:
    /// `product_id` as `parse_proximity_data` reads it, fresh keys, in range
    /// and disconnected. `name` is the pods' own, which the fake BlueZ shows
    /// as `Name` and, with no alias set, `Alias`.
    pub async fn add_pods(&self, mac: Address, product_id: u16, name: &str) -> Result<()> {
        let adapters = self.0.bluez.adapters();
        let adapter = adapters.first().context("the world has no adapter")?;
        self.add_pods_on(adapter, mac, product_id, name).await
    }

    /// [`World::add_pods`], with the adapter called `adapter`.
    pub async fn add_pods_on(
        &self,
        adapter: &str,
        mac: Address,
        product_id: u16,
        name: &str,
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
        self.0.bluez.add_device(adapter, mac, name).await?;
        let mut pods = Pods::new(product_id, mac);
        pods.set_name(name);
        pairs.insert(
            mac,
            Pair {
                pods,
                link: None,
                aap_attempts: 0,
            },
        );
        tokio::spawn(self.clone().accept(mac, listener));
        tracing::info!("fake world: {name} paired at {mac} with {adapter}");
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

    /// Disconnects a pair, and with it the AAP connection - this host's link
    /// going, or another host taking the pair over. On the hardware AAP dies
    /// with the link, in the same millisecond as BlueZ's `Connected=false`
    /// and in either order: after it on a local disconnect, ahead of it when
    /// another host took the pair over (#71). The fake picks BlueZ first, then
    /// the AAP connection closes.
    ///
    /// The hardware's AAP ends with a read error, `Connection reset by peer`.
    /// Over the fake's Unix socket the app reads a close instead: a Unix
    /// socket resets its peer only when it closes with something of the
    /// peer's unread, and the app sends nothing unasked. The app takes the
    /// two alike, as the link gone (`aap::client::is_link_gone`).
    pub async fn disconnect(&self, mac: Address) -> Result<()> {
        self.set_connected(mac, false).await
    }

    async fn set_connected(&self, mac: Address, connected: bool) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        if connected {
            // bluetoothd reads the pair's name as it connects: a rename on
            // the pods shows in `Name` from here (#160). Ahead of
            // `Connected`, the fake's choice, so that whoever reads the names
            // on `Connected=true` reads this one.
            if self.0.bluez.can_connect(mac) {
                let name = String::from_utf8_lossy(pair.pods.name()).into_owned();
                self.0.bluez.set_name(mac, &name).await?;
            }
            // BlueZ first, as it turns away a pair whose adapter is off.
            self.0.bluez.set_connected(mac, true).await?;
            pair.pods.set_connected(true);
            self.flush(mac, pair, self.now()).await;
        } else {
            // BlueZ first, then AAP: see `World::disconnect`.
            self.0.bluez.set_connected(mac, false).await?;
            pair.pods.set_connected(false);
            // Disconnected, the model hangs up an open connection.
            self.flush(mac, pair, self.now()).await;
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
        // BlueZ first, then AAP, as on a disconnect.
        self.0.bluez.remove_adapter(name).await?;
        let paired = self.0.bluez.paired_with(name);
        self.drop_links(&mut pairs, &paired).await;
        tracing::info!("fake world: adapter {name} unplugged");
        Ok(())
    }

    /// Powers an adapter on or off - see [`FakeBluez::set_powered`]. Off, its
    /// pairs lose their link, and AAP with it; on again, they stay
    /// disconnected until connected.
    pub async fn set_powered(&self, name: &str, powered: bool) -> Result<()> {
        let mut pairs = self.0.pairs.lock().await;
        // BlueZ first, then AAP, as on a disconnect.
        self.0.bluez.set_powered(name, powered).await?;
        if !powered {
            let paired = self.0.bluez.paired_with(name);
            self.drop_links(&mut pairs, &paired).await;
        }
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

    /// Disconnects the models of the pairs at `macs`, which hangs up their
    /// AAP connections. BlueZ is the caller's to tell, first.
    async fn drop_links(&self, pairs: &mut BTreeMap<Address, Pair>, macs: &[Address]) {
        let now = self.now();
        for &mac in macs {
            if let Some(pair) = pairs.get_mut(&mac)
                && pair.pods.is_connected()
            {
                pair.pods.set_connected(false);
                self.flush(mac, pair, now).await;
            }
        }
    }

    /// Carries out what a pair's model has due by `now`: packets and a
    /// hang-up on its AAP connection, and a link it drops - BlueZ told of it,
    /// then the connection closed.
    async fn flush(&self, mac: Address, pair: &mut Pair, now: Duration) {
        for action in pair.pods.poll(now) {
            if action != Action::Disconnect {
                pair.carry_out(action);
                continue;
            }
            if let Err(e) = self.0.bluez.set_connected(mac, false).await {
                tracing::warn!("fake world: failed to mark {mac} disconnected: {e:#}");
            }
            pair.carry_out(Action::HangUp);
            tracing::info!("fake world: {mac} dropped its link, both pods in the closed case");
        }
    }

    /// The user of a pair speaks for `length`, from now - see
    /// [`Pods::speak`]. With Conversation Awareness off the pods do nothing.
    /// Fails with no AAP connection open, and while a stretch is under way.
    pub async fn speak(&self, mac: Address, length: Duration) -> Result<()> {
        self.with_pods_at(mac, |pods, now| pods.speak(now, length))
            .await?
            .map_err(|e| anyhow::anyhow!("{mac}: {e}"))?;
        tracing::info!("fake world: {mac} hears speech for {length:?}");
        Ok(())
    }

    /// Starts or stops audio to a connected pair, as the audio server does -
    /// see [`FakeBluez::set_playing`].
    pub async fn set_playing(&self, mac: Address, playing: bool) -> Result<()> {
        self.0.bluez.set_playing(mac, playing).await?;
        tracing::info!(
            "fake world: audio to {mac} {}",
            if playing { "plays" } else { "stopped" }
        );
        Ok(())
    }

    /// Starts or ends a call on a connected pair - see
    /// [`FakeBluez::set_call`].
    pub async fn set_call(&self, mac: Address, on: bool) -> Result<()> {
        self.0.bluez.set_call(mac, on).await?;
        tracing::info!(
            "fake world: a call on {mac} {}",
            if on { "begins" } else { "ends" }
        );
        Ok(())
    }

    /// Sets a pair's A2DP volume as the user does - see
    /// [`FakeBluez::set_volume`].
    pub async fn set_volume(&self, mac: Address, volume: u16) -> Result<()> {
        self.0.bluez.set_volume(mac, volume).await?;
        tracing::info!("fake world: the user set the volume of {mac} to {volume}");
        Ok(())
    }

    /// Has a pair answer a volume the app sets as `answer` says - see
    /// [`FakeBluez::set_volume_answer`].
    pub fn set_volume_answer(&self, mac: Address, answer: VolumeAnswer) -> Result<()> {
        self.0.bluez.set_volume_answer(mac, answer)
    }

    /// The volume of a pair's A2DP transport, `None` without one.
    pub async fn volume(&self, mac: Address) -> Option<u16> {
        self.0.bluez.volume(mac).await
    }

    /// Every volume the app set on a pair's transports, in order, with when.
    pub fn volume_sets(&self, mac: Address) -> Vec<(Instant, u16)> {
        self.0.bluez.volume_sets(mac)
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
        self.with_pods_at(mac, |pods, _| change(pods)).await
    }

    /// [`World::with_pods`], with `change` handed the time it is made at,
    /// read with the pairs locked.
    async fn with_pods_at<R>(
        &self,
        mac: Address,
        change: impl FnOnce(&mut Pods, Duration) -> R,
    ) -> Result<R> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        let now = self.now();
        let result = change(&mut pair.pods, now);
        self.flush(mac, pair, now).await;
        self.0.due.notify_one();
        Ok(result)
    }

    /// Sets a pair's BlueZ alias, or clears it - see
    /// [`FakeBluez::set_alias`].
    pub async fn set_alias(&self, mac: Address, alias: Option<&str>) -> Result<()> {
        anyhow::ensure!(self.0.pairs.lock().await.contains_key(&mac), no_pair(mac));
        self.0.bluez.set_alias(mac, alias).await
    }

    /// The pods' own name, as they hold it - see [`Pods::name`].
    pub async fn name(&self, mac: Address) -> Result<Vec<u8>> {
        self.with_pods(mac, |pods| pods.name().to_vec()).await
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

    /// The addresses a pair advertises from now, one for each pod that
    /// advertises: none while its pods are quiet in the closed case.
    pub async fn advertising(&self, mac: Address) -> Result<Vec<Address>> {
        let mut pairs = self.0.pairs.lock().await;
        let pair = pairs.get_mut(&mac).ok_or_else(|| no_pair(mac))?;
        let adverts = pair.pods.advertisements(self.now());
        Ok(adverts.into_iter().map(|advert| advert.address).collect())
    }

    /// What GNOME Settings shows: each device's level, by MAC.
    pub fn batteries(&self) -> HashMap<Address, u8> {
        self.0.bluez.batteries()
    }

    /// `count` advertisers no key decrypts, as a BLE spam flood brings: each a
    /// pair with keys nobody holds, so its advertisement parses but stays
    /// closed, and its address resolves with no IRK the app holds,
    /// advertising once, from an address of its own. A new address is
    /// signaled only as it appears, with `InterfacesAdded`: the scanner reads
    /// it, attachment's connection watch does not. Gone after the fake
    /// BlueZ's timeout.
    pub async fn flood(&self, count: u32) -> Result<()> {
        self.spam(count, 1).await
    }

    /// `count` advertisers no key decrypts and no IRK resolves, as
    /// [`World::flood`] brings, each advertising the same payload `times`
    /// times from one address. Every advertisement after an advertiser's
    /// first is signaled as a change of strength alone, with
    /// `PropertiesChanged`, which attachment's connection watch subscribes to
    /// as well as the scanner.
    pub async fn flood_repeating(&self, count: u32, times: u32) -> Result<()> {
        self.spam(count, times).await
    }

    async fn spam(&self, count: u32, times: u32) -> Result<()> {
        let now = self.now();
        for _ in 0..count {
            let spammer = Pods::new(PRO_3, random_address());
            let advert = spammer.advertisement(PodSide::Right, now);
            let address = advert.address;
            for _ in 0..times {
                self.0.bluez.advertise(address, &advert.data).await?;
            }
        }
        tracing::debug!("fake world: flooded with {count} advertisers, {times} time(s) each");
        Ok(())
    }

    /// Advertises every pod that advertises every [`ADVERTISING_INTERVAL`],
    /// and has the fake BlueZ forget the addresses that went quiet.
    async fn advertise(self) {
        let mut tick = tokio::time::interval(ADVERTISING_INTERVAL);
        loop {
            tick.tick().await;
            let adverts: Vec<_> = {
                let mut pairs = self.0.pairs.lock().await;
                let now = self.now();
                pairs
                    .iter_mut()
                    .flat_map(|(mac, pair)| {
                        let adverts = pair.pods.advertisements(now);
                        adverts.into_iter().map(|advert| (*mac, advert))
                    })
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
            for (mac, pair) in pairs.iter_mut() {
                self.flush(*mac, pair, now).await;
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
        tracing::info!("fake world: AAP connected to {mac}");

        // There is no AAP without a Bluetooth link, so the connection brought
        // one up, as an L2CAP connect does. Ahead of what the model has due,
        // which may be to drop it again.
        if !was_connected && let Err(e) = self.0.bluez.set_connected(mac, true).await {
            tracing::warn!("fake world: failed to mark {mac} connected: {e:#}");
        }
        self.flush(mac, pair, now).await;
        self.0.due.notify_one();
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
                    self.flush(mac, pair, now).await;
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

fn parse_pod(pod: &str) -> fdo::Result<PodSide> {
    match pod {
        "left" => Ok(PodSide::Left),
        "right" => Ok(PodSide::Right),
        _ => Err(fdo::Error::InvalidArgs(format!(
            "no pod {pod:?}: left or right"
        ))),
    }
}

fn parse_part(part: &str) -> Result<Part, String> {
    match part {
        "left" => Ok(Part::Left),
        "right" => Ok(Part::Right),
        "case" => Ok(Part::Case),
        _ => Err(format!("no part {part:?}: left, right or case")),
    }
}

/// Sets the quirk named by its field in [`Quirks`]. The durations go by
/// their name with `_ms` - `echo_delay_ms`, `handshake_delay_ms`,
/// `report_delay_ms`, `dump_delay_ms` and `stale_report_ms` - in
/// milliseconds, a `stale_report_ms` of 0 turning the quirk off. A number may
/// come as any integer type, as `gdbus` sends `<4>` as an `int32`.
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
        "idle_level_only" => quirks.idle_level_only = flag()?,
        "echo_every" => quirks.echo_every = count()?,
        "echo_delay_ms" => quirks.echo_delay = Duration::from_millis(count()?.into()),
        "silent_handshake" => quirks.silent_handshake = flag()?,
        "refuse_aap" => quirks.refuse_aap = count()?,
        "handshake_delay_ms" => quirks.handshake_delay = Duration::from_millis(count()?.into()),
        "report_delay_ms" => quirks.report_delay = Duration::from_millis(count()?.into()),
        "dump_delay_ms" => quirks.dump_delay = Duration::from_millis(count()?.into()),
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
    async fn add_pods(&self, mac: &str, model: u16, name: &str) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world.add_pods(mac, model, name).await.map_err(failed)
    }

    async fn add_pods_on(
        &self,
        adapter: &str,
        mac: &str,
        model: u16,
        name: &str,
    ) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .add_pods_on(adapter, mac, model, name)
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

    async fn set_in_ear(&self, mac: &str, pod: &str, in_ear: bool) -> fdo::Result<()> {
        let (mac, pod) = (parse_mac(mac)?, parse_pod(pod)?);
        self.world
            .with_pods(mac, |pods| pods.set_in_ear(pod, in_ear))
            .await
            .map_err(failed)
    }

    async fn set_in_case(&self, mac: &str, pod: &str, in_case: bool) -> fdo::Result<()> {
        let (mac, pod) = (parse_mac(mac)?, parse_pod(pod)?);
        self.world
            .with_pods(mac, |pods| pods.set_in_case(pod, in_case))
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

    async fn set_alias(&self, mac: &str, alias: &str) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world.set_alias(mac, Some(alias)).await.map_err(failed)
    }

    async fn name(&self, mac: &str) -> fdo::Result<String> {
        let name = self.world.name(parse_mac(mac)?).await.map_err(failed)?;
        Ok(String::from_utf8_lossy(&name).into_owned())
    }

    async fn set_conversation_awareness(&self, mac: &str, enabled: bool) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        self.world
            .with_pods(mac, |pods| pods.set_conversation_awareness(enabled))
            .await
            .map_err(failed)
    }

    async fn speak(&self, mac: &str, length_ms: u32) -> fdo::Result<()> {
        let length = Duration::from_millis(length_ms.into());
        self.world
            .speak(parse_mac(mac)?, length)
            .await
            .map_err(failed)
    }

    async fn set_playing(&self, mac: &str, playing: bool) -> fdo::Result<()> {
        self.world
            .set_playing(parse_mac(mac)?, playing)
            .await
            .map_err(failed)
    }

    async fn set_call(&self, mac: &str, on: bool) -> fdo::Result<()> {
        self.world
            .set_call(parse_mac(mac)?, on)
            .await
            .map_err(failed)
    }

    async fn set_volume(&self, mac: &str, volume: u16) -> fdo::Result<()> {
        let mac = parse_mac(mac)?;
        if volume > crate::volume::MAX_VOLUME {
            return Err(fdo::Error::InvalidArgs(format!(
                "{volume} is above {}",
                crate::volume::MAX_VOLUME
            )));
        }
        self.world.set_volume(mac, volume).await.map_err(failed)
    }

    async fn set_volume_answer(&self, mac: &str, delay_ms: u32, to_even: bool) -> fdo::Result<()> {
        let answer = VolumeAnswer {
            delay: Duration::from_millis(delay_ms.into()),
            to_even,
        };
        self.world
            .set_volume_answer(parse_mac(mac)?, answer)
            .map_err(failed)
    }

    async fn volume(&self, mac: &str) -> fdo::Result<u16> {
        let mac = parse_mac(mac)?;
        self.world
            .volume(mac)
            .await
            .ok_or_else(|| fdo::Error::Failed(format!("{mac} has no A2DP transport")))
    }

    async fn volume_sets(&self, mac: &str) -> fdo::Result<Vec<u16>> {
        let mac = parse_mac(mac)?;
        Ok(self
            .world
            .volume_sets(mac)
            .into_iter()
            .map(|(_, volume)| volume)
            .collect())
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
    use crate::aap::ear::Place;
    use crate::aap::noise::{self, NoiseMode};
    use crate::aap::{Client, parse_battery_packet};
    use crate::ble::decrypt::decrypt_for_device;
    use crate::ble::rpa::resolves;
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

    /// Runs the handshake, and reads what the pair sends unasked with its
    /// answer: the hosts it knows, 0x2B.
    async fn handshake(client: &Client) {
        client.handshake().await.unwrap();
        assert_eq!(next_packet(client).await.unwrap()[4], 0x2B);
    }

    /// Whether the fake BlueZ shows the pair connected, read afresh.
    async fn bluez_connected(bus: &PrivateBus, mac: Address) -> bool {
        device_proxy(bus, mac)
            .await
            .get_property("Connected")
            .await
            .unwrap()
    }

    /// The pair's `Name` and `Alias` in the fake BlueZ, read afresh.
    async fn bluez_names(bus: &PrivateBus, mac: Address) -> (String, String) {
        let device = device_proxy(bus, mac).await;
        (
            device.get_property("Name").await.unwrap(),
            device.get_property("Alias").await.unwrap(),
        )
    }

    /// The pair's `Device1` in the fake BlueZ, its properties uncached.
    async fn device_proxy(bus: &PrivateBus, mac: Address) -> zbus::Proxy<'static> {
        let conn = bus.connect().await.unwrap();
        let path = device_path(DEFAULT_ADAPTER, mac);
        zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination(BLUEZ_SERVICE)
            .unwrap()
            .path(path)
            .unwrap()
            .interface("org.bluez.Device1")
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
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
        set_quirk(&mut quirks, "idle_level_only", &Value::from(true)).unwrap();
        assert!(quirks.silent_handshake);
        assert!(quirks.idle_level_only);
        assert_eq!(quirks.refuse_aap, 3);
        assert_eq!(quirks.echo_every, 1);
        assert_eq!(quirks.echo_delay, Duration::from_millis(20));
        set_quirk(&mut quirks, "handshake_delay_ms", &Value::from(1200u32)).unwrap();
        set_quirk(&mut quirks, "dump_delay_ms", &Value::from(1500i32)).unwrap();
        assert_eq!(quirks.handshake_delay, Duration::from_millis(1200));
        assert_eq!(quirks.dump_delay, Duration::from_millis(1500));
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

        // Back once the pair has answered it, and sent the hosts it knows.
        handshake(&client).await;
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

    /// BlueZ reads the pods' name as the pair connects, not before: renamed
    /// while connected, `Name` and `Alias` keep the old name until the next
    /// connection, and then both take the new one while no alias is set
    /// (#160). An alias set stays, whatever the pods are called, and cleared,
    /// the alias is the name again. A rename over AAP, and the pods taking
    /// it, is the pipeline's to drive.
    #[tokio::test]
    async fn bluez_reads_the_pods_name_as_they_connect() {
        let (bus, world, _dir) = world().await;
        world.connect(MAC).await.unwrap();
        world
            .with_pods(MAC, |pods| pods.set_name("Test Pods"))
            .await
            .unwrap();
        let fake = "Fake AirPods".to_string();
        assert_eq!(bluez_names(&bus, MAC).await, (fake.clone(), fake));

        world.disconnect(MAC).await.unwrap();
        world.connect(MAC).await.unwrap();
        let renamed = "Test Pods".to_string();
        assert_eq!(
            bluez_names(&bus, MAC).await,
            (renamed.clone(), renamed.clone())
        );

        world.set_alias(MAC, Some("Kitchen")).await.unwrap();
        assert_eq!(
            bluez_names(&bus, MAC).await,
            (renamed.clone(), "Kitchen".to_string())
        );
        world.set_alias(MAC, None).await.unwrap();
        assert_eq!(bluez_names(&bus, MAC).await, (renamed.clone(), renamed));
    }

    /// Disconnecting takes the AAP connection along, and BlueZ says so first:
    /// once the connection has closed, BlueZ has the pair disconnected, though
    /// the disconnect is not done.
    #[tokio::test]
    async fn disconnecting_closes_the_connection() {
        let (bus, world, dir) = world().await;
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        handshake(&client).await;

        let disconnecting = tokio::spawn({
            let world = world.clone();
            async move { world.disconnect(MAC).await }
        });
        assert!(next_packet(&client).await.is_err());
        assert!(!bluez_connected(&bus, MAC).await, "closed ahead of BlueZ");
        disconnecting.await.unwrap().unwrap();
    }

    /// Both pods in the closed case: the pair drops its link, and BlueZ says
    /// so, as the AAP connection goes with it.
    #[tokio::test]
    async fn the_closed_case_drops_the_link() {
        let (bus, world, dir) = world().await;
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        handshake(&client).await;

        world
            .with_pods(MAC, |pods| pods.set_lid(false))
            .await
            .unwrap();
        assert!(next_packet(&client).await.is_err());
        assert!(!bluez_connected(&bus, MAC).await);
        assert!(
            !world
                .with_pods(MAC, |pods| pods.is_connected() || pods.has_aap())
                .await
                .unwrap()
        );
    }

    /// An adapter powering off takes its pairs' links, and AAP with them, and
    /// nothing connects until it is on again. Unplugged, it takes them too.
    #[tokio::test]
    async fn an_adapter_going_takes_its_pairs_links() {
        let (bus, world, dir) = world().await;
        world.connect(MAC).await.unwrap();
        let client = aap(&dir, MAC).await;
        handshake(&client).await;

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
        handshake(&client).await;

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
        handshake(&first).await;

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
        handshake(&client).await;

        client.set_noise_mode(NoiseMode::Adaptive).await.unwrap();
        assert_eq!(
            next_packet(&client).await.unwrap(),
            noise::set_packet(NoiseMode::Adaptive)
        );
    }

    /// Every pair advertises while the app discovers, from an address its
    /// IRK resolves, and its key decrypts what it sends.
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
                    let resolved = resolves(&keys.irk, advert.ble_mac);
                    let decrypted = decrypt_for_device(encrypted, &keys.enc_key, *mac).is_ok();
                    assert_eq!(resolved, decrypted, "{mac}'s IRK and key disagree");
                    if decrypted {
                        heard.insert(*mac);
                    }
                }
            }
        })
        .await
        .expect("heard both pairs");
    }

    /// A flood's advertisers parse, come each from an address of its own, and
    /// the pair's key decrypts none of them, nor does its IRK resolve their
    /// addresses.
    #[tokio::test]
    async fn a_flood_decrypts_with_no_key() {
        let (bus, world, _dir) = world().await;
        let scanner = Scanner::with_connection(bus.connect().await.unwrap());
        let adverts = scanner.advertisements().await.unwrap();
        tokio::pin!(adverts);
        scanner.start_discovery().await.unwrap();
        let keys = world.keys(MAC).await.unwrap();

        world.flood(3).await.unwrap();
        let mut addresses = HashSet::new();
        // By address, so that three are three addresses of their own.
        timeout(TIMEOUT, async {
            while addresses.len() < 3 {
                let advert = adverts.next().await.unwrap();
                let encrypted = advert.data.encrypted_portion().unwrap();
                if decrypt_for_device(encrypted, &keys.enc_key, MAC).is_err() {
                    assert!(!resolves(&keys.irk, advert.ble_mac));
                    addresses.insert(advert.ble_mac);
                }
            }
        })
        .await
        .expect("heard the flood, from three addresses");
    }

    /// A flood is signaled only as its addresses appear, and a repeating
    /// flood as changes after that: of the strength alone, as an advertiser
    /// repeating its payload is (#71). Those are what attachment's connection
    /// watch subscribes to, and the #90 test relies on. A world with no pair
    /// in it, so that every device signaled is an advertiser's.
    #[tokio::test]
    async fn a_repeating_flood_is_signaled_as_changes_of_strength() {
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
                let (iface, props, _) = msg
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    .ok()?;
                let path = msg.header().path()?.to_string();
                let mut changed: Vec<String> = props.into_keys().collect();
                changed.sort_unstable();
                (iface == "org.bluez.Device1").then_some((path, changed))
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
                let (device, properties) = changes.next().await.unwrap();
                assert_eq!(properties, ["RSSI"], "{device}: the payload repeated");
                *changed.entry(device).or_default() += 1;
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
        control
            .call_method("SetInEar", &(&other, "left", true))
            .await
            .unwrap();
        control
            .call_method("SetInCase", &(&other, "right", false))
            .await
            .unwrap();
        let places = world
            .with_pods(OTHER, |pods| {
                (pods.place_of(PodSide::Left), pods.place_of(PodSide::Right))
            })
            .await
            .unwrap();
        assert_eq!(places, (Place::InEar, Place::Out));
        control
            .call_method("SetConversationAwareness", &(&other, true))
            .await
            .unwrap();
        let name: String = control.call("Name", &(&other,)).await.unwrap();
        assert_eq!(name, "Other");
        control
            .call_method("SetAlias", &(&other, "Kitchen"))
            .await
            .unwrap();
        assert_eq!(bluez_names(&bus, OTHER).await.1, "Kitchen");
        control
            .call_method("SetAlias", &(&other, ""))
            .await
            .unwrap();
        assert_eq!(bluez_names(&bus, OTHER).await.1, "Other");
        let (left, case, echo_every, repaired, conversation_awareness) = world
            .with_pods(OTHER, |pods| {
                (
                    pods.battery(Part::Left),
                    pods.battery(Part::Case),
                    pods.quirks.echo_every,
                    pods.keys() != &keys,
                    pods.conversation_awareness(),
                )
            })
            .await
            .unwrap();
        assert_eq!((left, case, echo_every), (Some(42), None, 2));
        assert!(repaired);
        assert!(conversation_awareness);

        control.call_method("Connect", &(&other,)).await.unwrap();
        assert!(bluez_connected(&bus, OTHER).await);
        // Audio, as the audio server and the user change it.
        control
            .call_method("SetPlaying", &(&other, true))
            .await
            .unwrap();
        control
            .call_method("SetVolume", &(&other, 90u16))
            .await
            .unwrap();
        let volume: u16 = control.call("Volume", &(&other,)).await.unwrap();
        assert_eq!(volume, 90);
        control
            .call_method("SetVolumeAnswer", &(&other, 150u32, true))
            .await
            .unwrap();
        let answer = world.bluez().volume_answer(OTHER).expect("a paired device");
        assert_eq!(
            (answer.delay, answer.to_even),
            (Duration::from_millis(150), true)
        );
        control
            .call_method("SetCall", &(&other, true))
            .await
            .unwrap();
        control
            .call_method("SetCall", &(&other, false))
            .await
            .unwrap();
        let sets: Vec<u16> = control.call("VolumeSets", &(&other,)).await.unwrap();
        assert!(sets.is_empty(), "the user's are not the app's");
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
                    .call_method("SetInEar", &(&other, "middle", true))
                    .await,
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
                control.call_method("SetVolume", &(&other, 128u16)).await,
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            (
                control.call_method("Volume", &(&other,)).await,
                "org.freedesktop.DBus.Error.Failed",
            ),
            (
                control.call_method("SetPlaying", &(&other, true)).await,
                "org.freedesktop.DBus.Error.Failed",
            ),
            (
                control.call_method("Speak", &(&other, 3000u32)).await,
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
