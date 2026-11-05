//! Pausing the media when a pod leaves an ear, and playing it again once it
//! is back (#167): the host's half of ear detection, as an iPhone or a Mac
//! does it. The pods only report where they are; that they pause nothing
//! themselves is believed, not checked, and none of this has been tried on a
//! pair yet (#167).
//!
//! A pod that was in an ear leaving it, while the pair's A2DP transport is
//! `active`, pauses every MPRIS player that is `Playing` on the session bus,
//! and the players paused and the pods worn then are remembered. Once every
//! pod worn at the pause is back in an ear, each remembered player that is
//! still `Paused` plays again - or still `Playing`, within [`PAUSE_SETTLE`]
//! of the pause, its pause perhaps not reported yet - and every remembered
//! player is forgotten. A pod going into the case, the link going, or the
//! setting turned off forgets the pause, and a player reading anything else
//! when the pods are back is the user's and is left alone.
//! `docs/app/media-pause.md` says how and why, and what is assumed.
//!
//! [`EarPause`] decides, from the pods' places, whether the transport is
//! active, the players' statuses and the time, as `volume::Ducking` does, and
//! is tested on its own. [`Media::run`] carries it out over D-Bus, in a task
//! of its own beside attachment - not in a pair's supervisor, which is
//! aborted wherever it is when the pair detaches. It takes the pods' places
//! as events from the coordinator, [`Coordinator::ear_events`], which come
//! from the link's ear state alone, never from the advertisement - see
//! [`EarEvent`]. It reads only what a decision needs, takes no signal, and
//! makes every call without auto-start and within [`CALL_LIMIT`]. A player
//! that refuses `Pause` is not remembered as paused.
//!
//! Explicit `Pause` and `Play`, never `PlayPause`: a toggle sent while
//! nothing plays, or after the user paused by hand, would start the media,
//! and a pod coming out must never start anything.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::future::join_all;
use tokio::sync::{mpsc, oneshot, watch};
use zbus::Connection;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::zvariant::OwnedValue;

use crate::aap::ear::Place;
use crate::podstate::{Coordinator, EarEvent};
use crate::volume::{Connect, a2dp_active_in, bluez_objects};

/// How long after a pause a remembered player that still reads `Playing` is
/// taken as on its way to `Paused`, not as played again by the user: it is
/// sent `Play` all the same, which leaves a playing player as it is. A guess
/// at how long a player takes to report its pause - a browser presumably
/// hands it to the page first, not checked - not measured (#167).
pub const PAUSE_SETTLE: Duration = Duration::from_secs(2);

/// The longest one call on the session bus or BlueZ's, or making a
/// connection, may take: a guess, as `volume::CALL_LIMIT` is. A player that
/// takes longer is left out, or left as it is, and the others are not held
/// up: they are called side by side. The app's; a test on a loaded machine
/// may give [`Media`] longer.
pub const CALL_LIMIT: Duration = Duration::from_secs(1);

/// What every MPRIS player's name on the session bus starts with.
pub const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Where every MPRIS player serves its interfaces.
pub const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// The interface with `PlaybackStatus`, `Pause` and `Play`.
pub const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// A player's `PlaybackStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
    Stopped,
}

impl Playback {
    /// The status MPRIS names `name`; `None` for any other.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "Playing" => Some(Self::Playing),
            "Paused" => Some(Self::Paused),
            "Stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    /// The name MPRIS gives it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

/// The Preferences switch as it reaches [`Media`] on a watch: whether it is
/// on, and how many times it has been turned off. A watch keeps the latest
/// value alone, so a switch turned off and on again before the task looked
/// would read as on throughout; the count tells it was off meanwhile, and
/// what was paused is forgotten all the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    pub on: bool,
    /// How many times it was turned off since the app started.
    pub offs: u64,
}

impl Switch {
    /// As saved, never turned off yet.
    pub fn new(on: bool) -> Self {
        Self { on, offs: 0 }
    }

    /// Turns it on or off; turned off from on, it counts.
    pub fn set(&mut self, on: bool) {
        if self.on && !on {
            self.offs += 1;
        }
        self.on = on;
    }
}

/// An MPRIS player, as read from the session bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    /// Its connection's unique name, which it is called at: no other
    /// connection takes it after the player closes, so a player started
    /// again under the same MPRIS name is another.
    pub id: String,
    /// Its MPRIS name, such as `org.mpris.MediaPlayer2.spotify`: for the
    /// logs.
    pub name: String,
    pub status: Playback,
}

/// What [`EarPause`] asks to be done: players to pause, and players to play
/// again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Action {
    pub pause: Vec<Player>,
    pub play: Vec<Player>,
}

/// What [`EarPause::places`] will need to know about the pods' next places,
/// so the caller reads nothing a decision does not use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// Whether the pair's A2DP transport is `active`: a pod left an ear with
    /// nothing paused.
    pub transport: bool,
    /// The players and their statuses: for a pause, while the transport is
    /// active, and for playing again.
    pub players: bool,
}

/// What a change of places does.
#[derive(Debug, Clone, Copy, Default)]
struct Step {
    /// A pod went into the case: what was paused is forgotten.
    into_case: bool,
    /// A pod that was in an ear left it, with nothing paused, or a pause
    /// just forgotten: the media pauses.
    pause: bool,
    /// Every pod worn at the pause is back in an ear: the media plays again.
    play: bool,
}

/// What a pause remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paused {
    /// Which pods were in an ear just before the pause, left then right:
    /// each is to be back before anything plays again.
    worn: (bool, bool),
    /// The players it paused.
    players: Vec<Player>,
    /// When.
    at: Instant,
}

/// One pair's pause and play on its pods' places, from one link. Takes where
/// the pods are, whether the pair's A2DP transport is active, the players'
/// statuses and the time, and says what to pause and what to play; the
/// caller does it. What is remembered goes with the link, the setting turned
/// off, and a pod going into the case.
#[derive(Debug, Clone, Default)]
pub struct EarPause {
    /// The pods' last places, left then right: `None` until the link's first.
    places: Option<(Place, Place)>,
    paused: Option<Paused>,
}

impl EarPause {
    /// What the pods' places being `left` and `right` next will take - see
    /// [`Needs`].
    pub fn needs(&self, left: Place, right: Place) -> Needs {
        let step = self.step(left, right);
        Needs {
            transport: step.pause,
            players: step.pause || step.play,
        }
    }

    /// Takes the pods' places, left then right, that came at `now`, with
    /// whether the pair's A2DP transport is `active` and the players as they
    /// read now - each as [`EarPause::needs`] asked for, and `false` or none
    /// where it did not, or the read failed - and returns what to pause and
    /// what to play.
    ///
    /// A pause pauses every player that is `Playing`, and only while the
    /// transport is active: nothing plays to the pair otherwise, or a call
    /// is on. It remembers the players and the pods worn, unless it paused
    /// none, or a pod went into the case with it. Playing again plays the
    /// players remembered that read `Paused` - or `Playing`, within
    /// [`PAUSE_SETTLE`] of the pause - and forgets them all: one that reads
    /// anything else, or is gone, the user has taken over.
    pub fn places(
        &mut self,
        left: Place,
        right: Place,
        active: bool,
        players: &[Player],
        now: Instant,
    ) -> Action {
        let step = self.step(left, right);
        let before = self.places.replace((left, right));
        if step.into_case {
            self.paused = None;
        }
        if step.pause {
            if !active {
                return Action::default();
            }
            let pause: Vec<Player> = players
                .iter()
                .filter(|player| player.status == Playback::Playing)
                .cloned()
                .collect();
            if let Some((was_left, was_right)) = before
                && !step.into_case
                && !pause.is_empty()
            {
                self.paused = Some(Paused {
                    worn: (was_left == Place::InEar, was_right == Place::InEar),
                    players: pause.clone(),
                    at: now,
                });
            }
            return Action {
                pause,
                play: Vec::new(),
            };
        }
        if step.play
            && let Some(paused) = self.paused.take()
        {
            let settling = now.saturating_duration_since(paused.at) < PAUSE_SETTLE;
            let play = paused
                .players
                .iter()
                .filter_map(|remembered| {
                    players
                        .iter()
                        .find(|player| player.id == remembered.id)
                        .filter(|player| match player.status {
                            Playback::Paused => true,
                            Playback::Playing => settling,
                            Playback::Stopped => false,
                        })
                })
                .cloned()
                .collect();
            return Action {
                pause: Vec::new(),
                play,
            };
        }
        Action::default()
    }

    /// What the pods' places changing to `left` and `right` does.
    fn step(&self, left: Place, right: Place) -> Step {
        // The link's first places: nothing left an ear, or went into the
        // case.
        let Some((was_left, was_right)) = self.places else {
            return Step::default();
        };
        let pods = [(was_left, left), (was_right, right)];
        let into_case = pods
            .iter()
            .any(|(was, now)| !was.in_case() && now.in_case());
        // Into an unknown place is not out: nothing says what it is.
        let left_an_ear = pods.iter().any(|(was, now)| {
            *was == Place::InEar && *now != Place::InEar && !matches!(now, Place::Unknown(_))
        });
        let paused = self.paused.as_ref().filter(|_| !into_case);
        let pause = left_an_ear && paused.is_none();
        let play = paused.is_some_and(|paused| {
            (!paused.worn.0 || left == Place::InEar) && (!paused.worn.1 || right == Place::InEar)
        });
        Step {
            into_case,
            pause,
            play,
        }
    }

    /// Forgets what was paused: nothing plays again from it.
    pub fn forget(&mut self) {
        self.paused = None;
    }

    /// Takes the players with these IDs out of what was paused: their
    /// `Pause` was refused, or never sent, so they are not paused by this
    /// and are nothing to play again. With none left, nothing is remembered.
    pub fn not_paused(&mut self, ids: &[String]) {
        if let Some(paused) = &mut self.paused {
            paused.players.retain(|player| !ids.contains(&player.id));
            if paused.players.is_empty() {
                self.paused = None;
            }
        }
    }

    /// Whether players are remembered as paused, to play again once the
    /// pods are back.
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }
}

/// `call`, unless it takes longer than `limit` - [`CALL_LIMIT`] in the app.
async fn bounded<T>(
    limit: Duration,
    what: &str,
    call: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, call)
        .await
        .with_context(|| format!("no answer within {limit:?} {what}"))?
}

/// A proxy for the bus itself on `conn`.
async fn bus_proxy(conn: &Connection) -> Result<zbus::fdo::DBusProxy<'static>> {
    Ok(zbus::fdo::DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

/// The MPRIS players on the session bus at `conn`, each once, with its
/// status. One that does not answer within `limit` - [`CALL_LIMIT`] in the
/// app - or whose status is no MPRIS status, is left out; asked side by
/// side, none holds the others up.
pub async fn list_players(conn: &Connection, limit: Duration) -> Result<Vec<Player>> {
    let names: Vec<String> = bounded(limit, "listing the bus's names", async {
        let names: Option<Vec<String>> = bus_proxy(conn)
            .await?
            .inner()
            .call_with_flags("ListNames", MethodFlags::NoAutoStart.into(), &())
            .await
            .context("failed to list the bus's names")?;
        names.context("the bus sent no reply")
    })
    .await?;
    let players = join_all(
        names
            .into_iter()
            .filter(|name| name.starts_with(MPRIS_PREFIX))
            .map(|name| async move {
                let read = bounded(limit, "reading a player", read_player(conn, &name)).await;
                read.inspect_err(|e| tracing::debug!("{name} left out: {e:#}"))
                    .ok()
            }),
    )
    .await;
    // A player may hold several MPRIS names: it is called once.
    let mut seen = HashSet::new();
    Ok(players
        .into_iter()
        .flatten()
        .filter(|player| seen.insert(player.id.clone()))
        .collect())
}

/// The player that holds the MPRIS name `name`, with its status.
async fn read_player(conn: &Connection, name: &str) -> Result<Player> {
    let id: Option<String> = bus_proxy(conn)
        .await?
        .inner()
        .call_with_flags("GetNameOwner", MethodFlags::NoAutoStart.into(), &(name,))
        .await
        .context("gone")?;
    let id = id.context("the bus sent no reply")?;
    let properties = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(id.clone())?
        .path(MPRIS_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let status: Option<OwnedValue> = properties
        .inner()
        .call_with_flags(
            "Get",
            MethodFlags::NoAutoStart.into(),
            &(PLAYER_INTERFACE, "PlaybackStatus"),
        )
        .await
        .context("failed to read its PlaybackStatus")?;
    let status = status.context("it sent no reply")?;
    let status = String::try_from(status).context("its PlaybackStatus is no string")?;
    let status = Playback::parse(&status)
        .with_context(|| format!("its PlaybackStatus {status:?} is none of MPRIS's"))?;
    Ok(Player {
        id,
        name: name.to_string(),
        status,
    })
}

/// Calls `method` - `Pause` or `Play` - on `player`, within `limit` -
/// [`CALL_LIMIT`] in the app.
pub async fn call(conn: &Connection, player: &Player, method: &str, limit: Duration) -> Result<()> {
    bounded(limit, &format!("to {method}"), async {
        let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
            .destination(player.id.clone())?
            .path(MPRIS_PATH)?
            .interface(PLAYER_INTERFACE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let _: Option<()> = proxy
            .call_with_flags(method, MethodFlags::NoAutoStart.into(), &())
            .await
            .with_context(|| format!("{} refused {method}", player.name))?;
        Ok(())
    })
    .await
    .with_context(|| format!("{} did not take {method}", player.name))
}

/// Whether `e` is an answer - an error a player, or the bus, sent back -
/// rather than the connection failing or taking too long.
fn is_answer(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<zbus::Error>(),
            Some(zbus::Error::MethodError(..) | zbus::Error::FDO(_))
        )
    })
}

/// A bus [`Media`] reaches, connected when first needed, and again after the
/// connection failed.
struct Bus {
    /// What is on it, for the logs: `BlueZ`, `the players`.
    what: &'static str,
    connect: Connect,
    conn: Option<Connection>,
}

impl Bus {
    fn new(what: &'static str, connect: Connect) -> Self {
        Self {
            what,
            connect,
            conn: None,
        }
    }

    /// The connection, made if there is none, within `limit`.
    async fn connection(&mut self, limit: Duration) -> Result<Connection> {
        if let Some(conn) = &self.conn {
            return Ok(conn.clone());
        }
        let what = self.what;
        let builder =
            (self.connect)().with_context(|| format!("failed to find the bus {what} is on"))?;
        let conn = tokio::time::timeout(limit, builder.build())
            .await
            .with_context(|| format!("no connection to the bus {what} is on in {limit:?}"))?
            .with_context(|| format!("failed to connect to the bus {what} is on"))?;
        self.conn = Some(conn.clone());
        Ok(conn)
    }

    /// Drops the connection after a failure that was no answer: it is made
    /// again next time.
    fn check<T>(&mut self, result: &Result<T>) {
        if let Err(e) = result
            && !is_answer(e)
        {
            self.conn = None;
        }
    }
}

/// What pauses the media when a pod leaves an ear, for every pair with a
/// link. Its task is [`Media::run`]; [`MediaControl`] asks it, for tests,
/// whether it has taken every event.
pub struct Media {
    events: mpsc::UnboundedReceiver<EarEvent>,
    /// The Preferences switch: whether to pause and play at all.
    switch: watch::Receiver<Switch>,
    /// How many times the switch had been turned off when last looked at.
    offs: u64,
    /// How long one call may take - see [`CALL_LIMIT`].
    limit: Duration,
    requests: mpsc::UnboundedReceiver<Request>,
    /// The system bus, where BlueZ is.
    bluez: Bus,
    /// The session bus, where the players are.
    session: Bus,
    /// Each linked pair's, from its link's first places.
    pairs: HashMap<Address, EarPause>,
}

/// What [`MediaControl`] asks of [`Media::run`].
enum Request {
    /// Say, once every event told so far is taken, whether nothing is
    /// remembered as paused.
    Settled(oneshot::Sender<bool>),
}

/// Asks [`Media::run`] how it stands.
#[derive(Clone)]
pub struct MediaControl {
    requests: mpsc::UnboundedSender<Request>,
}

impl MediaControl {
    /// Whether nothing is remembered as paused, once every ear event the
    /// coordinator told before this call has been taken, and every pause and
    /// play it brought has been called: for a test to see what came of an
    /// event, rather than wait and see. `false` once the task is gone.
    pub async fn settled(&self) -> bool {
        let (done, answer) = oneshot::channel();
        if self.requests.send(Request::Settled(done)).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }
}

impl Media {
    /// Pauses and plays on `coordinator`'s ear events, from now on, while
    /// `switch` is on: reading the transport on the bus `bluez` connects to,
    /// and the players on the bus `session` connects to.
    pub fn new(
        coordinator: &Coordinator,
        switch: watch::Receiver<Switch>,
        bluez: Connect,
        session: Connect,
    ) -> (Self, MediaControl) {
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let offs = switch.borrow().offs;
        let media = Self {
            events: coordinator.ear_events(),
            switch,
            offs,
            limit: CALL_LIMIT,
            requests,
            bluez: Bus::new("BlueZ", bluez),
            session: Bus::new("the media players", session),
            pairs: HashMap::new(),
        };
        (
            media,
            MediaControl {
                requests: requests_tx,
            },
        )
    }

    /// Gives each call `limit` in place of [`CALL_LIMIT`]: for a test on a
    /// loaded machine, where a pause given up on would fail it.
    #[cfg(feature = "harness")]
    pub fn with_call_limit(mut self, limit: Duration) -> Self {
        self.limit = limit;
        self
    }

    /// Takes the events until the coordinator goes. Never ends otherwise: a
    /// bus failing to answer costs a pause or a play, and the connection is
    /// made again at the next.
    ///
    /// Events come first, then the setting, then requests: a request is
    /// taken only with no event waiting, so [`MediaControl::settled`]
    /// answers for every event told before it. An event reads the switch as
    /// it is then, so one turned off before an event never pauses on it.
    pub async fn run(mut self) {
        let mut setting_open = true;
        let mut requests_open = true;
        loop {
            tokio::select! {
                biased;
                event = self.events.recv() => match event {
                    Some(event) => self.handle(event).await,
                    None => return,
                },
                changed = self.switch.changed(), if setting_open => match changed {
                    Ok(()) => {
                        let switch = *self.switch.borrow_and_update();
                        self.look_at(switch);
                    }
                    // Dropped: the last setting holds.
                    Err(_) => setting_open = false,
                },
                request = self.requests.recv(), if requests_open => match request {
                    Some(Request::Settled(answer)) => {
                        let settled = self.pairs.values().all(|pair| !pair.is_paused());
                        let _ = answer.send(settled);
                    }
                    None => requests_open = false,
                },
            }
        }
    }

    async fn handle(&mut self, event: EarEvent) {
        match event {
            // A new link starts afresh: its first places tell nothing left an
            // ear.
            EarEvent::LinkUp { mac } => {
                self.pairs.remove(&mac);
            }
            EarEvent::LinkGone { mac } => {
                if self.pairs.remove(&mac).is_some_and(|pair| pair.is_paused()) {
                    tracing::info!("Media paused for {mac} stays paused: its link went");
                }
            }
            EarEvent::Places { mac, left, right } => self.places(mac, left, right).await,
        }
    }

    /// Takes the switch as it reads now: turned off at all since it was last
    /// looked at, what was paused is forgotten, whether it is on again or
    /// not.
    fn look_at(&mut self, switch: Switch) {
        if switch.offs != self.offs {
            self.offs = switch.offs;
            self.forget_all();
        }
    }

    async fn places(&mut self, mac: Address, left: Place, right: Place) {
        let switch = *self.switch.borrow();
        self.look_at(switch);
        let pair = self.pairs.entry(mac).or_default();
        if !switch.on {
            pair.forget();
        }
        let needs = if switch.on {
            pair.needs(left, right)
        } else {
            Needs::default()
        };
        let read = if needs.transport {
            self.transport_active(mac).await
        } else {
            None
        };
        let active = read == Some(true);
        // A pause reads the players only while the transport is active.
        let players = if needs.players && (active || !needs.transport) {
            self.players().await
        } else {
            Vec::new()
        };
        let pair = self.pairs.entry(mac).or_default();
        let was_paused = pair.is_paused();
        let action = pair.places(left, right, active, &players, Instant::now());
        let forgotten = was_paused && !pair.is_paused() && action.play.is_empty();
        if read == Some(false) {
            tracing::debug!(
                "a pod of {mac} left an ear; nothing paused, as no A2DP transport of the \
                 pair is active"
            );
        }
        for player in &action.pause {
            tracing::info!("Pausing {}: a pod of {mac} left an ear", player.name);
        }
        for player in &action.play {
            tracing::info!("Playing {} again: the pods of {mac} are back", player.name);
        }
        if forgotten {
            tracing::info!(
                "Media paused for {mac} left paused: a pod went into the case, or the user \
                 took the players over"
            );
        }
        let refused = self.carry_out(&action.pause, "Pause").await;
        if !refused.is_empty()
            && let Some(pair) = self.pairs.get_mut(&mac)
        {
            pair.not_paused(&refused);
        }
        self.carry_out(&action.play, "Play").await;
    }

    /// Whether an A2DP transport of the pair is `active`, read now, with a
    /// `Volume` or not: `None` when it could not be read, which counts as
    /// not - nothing is paused that might not play to the pair.
    async fn transport_active(&mut self, mac: Address) -> Option<bool> {
        let limit = self.limit;
        let found = match self.bluez.connection(limit).await {
            Ok(conn) => {
                bounded(limit, "reading BlueZ's objects", async {
                    Ok(a2dp_active_in(&bluez_objects(&conn).await?, mac))
                })
                .await
            }
            Err(e) => Err(e),
        };
        self.bluez.check(&found);
        found
            .inspect_err(|e| tracing::warn!("nothing paused for {mac}: {e:#}"))
            .ok()
    }

    /// The players, read now. Unread, there are none: nothing is paused, and
    /// nothing remembered plays again.
    async fn players(&mut self) -> Vec<Player> {
        let listed = match self.session.connection(self.limit).await {
            Ok(conn) => list_players(&conn, self.limit).await,
            Err(e) => Err(e),
        };
        self.session.check(&listed);
        listed.unwrap_or_else(|e| {
            tracing::warn!("failed to read the media players: {e:#}");
            Vec::new()
        })
    }

    /// Calls `method` on every one of `players`, side by side, logging a
    /// failure. Returns the IDs of those it surely did not reach: the
    /// player answered with an error, or no connection was made to call it.
    /// One that did not answer in time is not among them: the call may have
    /// landed.
    async fn carry_out(&mut self, players: &[Player], method: &str) -> Vec<String> {
        if players.is_empty() {
            return Vec::new();
        }
        let conn = match self.session.connection(self.limit).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("{e:#}");
                return players.iter().map(|player| player.id.clone()).collect();
            }
        };
        let limit = self.limit;
        let calls = join_all(
            players
                .iter()
                .map(|player| call(&conn, player, method, limit)),
        )
        .await;
        let mut refused = Vec::new();
        for (player, called) in players.iter().zip(calls) {
            self.session.check(&called);
            if let Err(e) = called {
                tracing::warn!("{e:#}");
                if is_answer(&e) {
                    refused.push(player.id.clone());
                }
            }
        }
        refused
    }

    /// Forgets every pause: the setting was turned off.
    fn forget_all(&mut self) {
        if self.pairs.values().any(EarPause::is_paused) {
            tracing::info!("Media paused for a pod left paused: the setting was turned off");
        }
        self.pairs.values_mut().for_each(EarPause::forget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Place::{InCase, InClosedCase, InEar, Out};

    fn player(id: &str, status: Playback) -> Player {
        Player {
            id: format!(":1.{id}"),
            name: format!("{MPRIS_PREFIX}{id}"),
            status,
        }
    }

    fn playing(id: &str) -> Player {
        player(id, Playback::Playing)
    }

    fn paused(id: &str) -> Player {
        player(id, Playback::Paused)
    }

    fn ids(players: &[Player]) -> Vec<&str> {
        players.iter().map(|player| player.id.as_str()).collect()
    }

    /// A pair whose link has reported the pods at `left` and `right`.
    fn at(left: Place, right: Place) -> EarPause {
        let mut pause = EarPause::default();
        let action = pause.places(left, right, true, &[playing("1")], Instant::now());
        assert_eq!(action, Action::default(), "a link's first places");
        pause
    }

    /// The pods' places changing to `left` and `right` at `now`, with the
    /// transport `active` and `players`, each read only as `needs` asks.
    fn change(
        pause: &mut EarPause,
        (left, right): (Place, Place),
        active: bool,
        players: &[Player],
        now: Instant,
    ) -> Action {
        let needs = pause.needs(left, right);
        let active = needs.transport && active;
        let players = if needs.players && (active || !needs.transport) {
            players
        } else {
            &[]
        };
        pause.places(left, right, active, players, now)
    }

    /// Both pods worn, one taken out: what plays is paused. Back in: it
    /// plays again. Either pod.
    #[test]
    fn one_pod_out_pauses_and_back_in_plays() {
        let now = Instant::now();
        let later = now + Duration::from_secs(30);
        for (out, back) in [
            ((Out, InEar), (InEar, InEar)),
            ((InEar, Out), (InEar, InEar)),
        ] {
            let mut pause = at(InEar, InEar);
            let players = [playing("1"), paused("2"), player("3", Playback::Stopped)];
            let action = change(&mut pause, out, true, &players, now);
            assert_eq!(ids(&action.pause), [":1.1"], "{out:?}");
            assert!(action.play.is_empty());
            assert!(pause.is_paused());

            let players = [paused("1"), paused("2"), player("3", Playback::Stopped)];
            let action = change(&mut pause, back, true, &players, later);
            assert_eq!(ids(&action.play), [":1.1"], "only what it paused");
            assert!(action.pause.is_empty());
            assert!(!pause.is_paused());
        }
    }

    /// Only one pod worn, the other in the case: the worn one out pauses,
    /// back in plays. The pod in the case need not come out.
    #[test]
    fn one_pod_worn_and_the_other_in_the_case() {
        let now = Instant::now();
        for case in [InCase, InClosedCase] {
            let mut pause = at(InEar, case);
            let action = change(&mut pause, (Out, case), true, &[playing("1")], now);
            assert_eq!(ids(&action.pause), [":1.1"]);
            let action = change(&mut pause, (InEar, case), true, &[paused("1")], now);
            assert_eq!(ids(&action.play), [":1.1"], "{case:?}");
        }
    }

    /// Both taken out: paused at the first, and nothing more at the second.
    /// Plays once both are back, not when one is.
    #[test]
    fn both_out_pauses_once_and_plays_once_both_are_back() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        assert_eq!(ids(&action.pause), [":1.1"]);
        let action = change(&mut pause, (Out, Out), true, &[playing("2")], now);
        assert_eq!(action, Action::default(), "paused already");
        let action = change(&mut pause, (InEar, Out), true, &[paused("1")], now);
        assert_eq!(action, Action::default(), "one back");
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(ids(&action.play), [":1.1"]);
    }

    /// A pod that was not worn at the pause need not come back, nor stay
    /// out: the pods worn then are what is waited for.
    #[test]
    fn only_the_pods_worn_at_the_pause_are_waited_for() {
        let now = Instant::now();
        let mut pause = at(InEar, Out);
        change(&mut pause, (Out, Out), true, &[playing("1")], now);
        let action = change(&mut pause, (Out, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default(), "the right pod was not worn");
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(ids(&action.play), [":1.1"]);
    }

    /// A pod going into the case forgets the pause: the pods were put away,
    /// and taking one out later and wearing it plays nothing.
    #[test]
    fn a_pod_into_the_case_forgets_the_pause() {
        let now = Instant::now();
        for into_case in [InCase, InClosedCase] {
            let mut pause = at(InEar, InEar);
            change(&mut pause, (Out, InEar), true, &[playing("1")], now);
            let action = change(&mut pause, (into_case, InEar), true, &[paused("1")], now);
            assert_eq!(action, Action::default());
            assert!(!pause.is_paused(), "{into_case:?}");
            change(&mut pause, (Out, InEar), true, &[paused("1")], now);
            let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
            assert_eq!(action, Action::default(), "nothing played again");
        }

        // The other pod, which stayed worn, going in.
        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        let action = change(&mut pause, (Out, InCase), true, &[paused("1")], now);
        assert_eq!(action, Action::default(), "nothing more playing to pause");
        assert!(!pause.is_paused());
    }

    /// Straight from an ear into the case, as two changes in one report: the
    /// media pauses, and nothing is remembered to play again.
    #[test]
    fn from_an_ear_into_the_case_pauses_and_remembers_nothing() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(&mut pause, (InCase, InEar), true, &[playing("1")], now);
        assert_eq!(ids(&action.pause), [":1.1"]);
        assert!(!pause.is_paused());
    }

    /// The user's action wins: a remembered player played, stopped or
    /// closed meanwhile is forgotten, and plays nothing; one still paused
    /// plays. A pod's press counts as the user's: it reaches the player.
    #[test]
    fn a_player_the_user_took_over_is_left_alone() {
        let now = Instant::now();
        let later = now + PAUSE_SETTLE;
        let mut pause = at(InEar, InEar);
        let all = [playing("1"), playing("2"), playing("3"), playing("4")];
        let action = change(&mut pause, (Out, InEar), true, &all, now);
        assert_eq!(ids(&action.pause), [":1.1", ":1.2", ":1.3", ":1.4"]);

        // 1 played by the user, 2 stopped, 3 closed, 4 still paused; and a
        // player started meanwhile, which was never paused.
        let now_read = [
            playing("1"),
            player("2", Playback::Stopped),
            paused("4"),
            paused("5"),
        ];
        let action = change(&mut pause, (InEar, InEar), true, &now_read, later);
        assert_eq!(ids(&action.play), [":1.4"]);
        assert!(!pause.is_paused(), "all forgotten");
    }

    /// A player the user had paused before is not paused, nor remembered,
    /// and does not play when the pods are back.
    #[test]
    fn a_player_paused_already_is_not_played() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(&mut pause, (Out, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default());
        assert!(!pause.is_paused(), "nothing paused, nothing to remember");
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default());
    }

    /// A quick out and in: a player that still reads `Playing`, its pause
    /// not reported yet, is sent `Play` all the same, which leaves it
    /// playing whichever way it went. After [`PAUSE_SETTLE`], `Playing` is
    /// the user's.
    #[test]
    fn a_quick_out_and_in_plays_what_is_still_pausing() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        let quick = now + Duration::from_millis(600);
        let action = change(&mut pause, (InEar, InEar), true, &[playing("1")], quick);
        assert_eq!(ids(&action.play), [":1.1"]);

        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        let action = change(
            &mut pause,
            (InEar, InEar),
            true,
            &[playing("1")],
            now + PAUSE_SETTLE,
        );
        assert_eq!(action, Action::default());
    }

    /// Nothing plays to the pair - or a call is on, as is assumed to leave
    /// A2DP not active: nothing is paused, nor remembered, and the players
    /// are not even read.
    #[test]
    fn nothing_is_paused_while_the_transport_is_not_active() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let needs = pause.needs(Out, InEar);
        assert_eq!(
            needs,
            Needs {
                transport: true,
                players: true
            }
        );
        let action = pause.places(Out, InEar, false, &[playing("1")], now);
        assert_eq!(action, Action::default());
        assert!(!pause.is_paused());
        assert_eq!(pause.needs(InEar, InEar), Needs::default());
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default());
    }

    /// Playing again does not look at the transport: it may have gone idle
    /// after the pause.
    #[test]
    fn playing_again_needs_no_transport() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        assert_eq!(
            pause.needs(InEar, InEar),
            Needs {
                transport: false,
                players: true
            }
        );
        let action = pause.places(InEar, InEar, false, &[paused("1")], now);
        assert_eq!(ids(&action.play), [":1.1"]);
    }

    /// What reads nothing: a pod going in, out of the case, or into it with
    /// nothing paused; a report that changes nothing; and a link's first.
    #[test]
    fn most_changes_read_nothing() {
        let pause = at(InEar, InCase);
        for (left, right) in [(InEar, Out), (InEar, InClosedCase), (InEar, InCase)] {
            assert_eq!(
                pause.needs(left, right),
                Needs::default(),
                "{left:?} {right:?}"
            );
        }
        assert_eq!(EarPause::default().needs(Out, Out), Needs::default());
    }

    /// A place with no meaning is not out of the ear: nothing pauses on it.
    #[test]
    fn an_unknown_place_pauses_nothing() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(
            &mut pause,
            (Place::Unknown(0x09), InEar),
            true,
            &[playing("1")],
            now,
        );
        assert_eq!(action, Action::default());
    }

    /// A role switch, as the link's ear state gives it: the primary pod out
    /// is one change, and the switch that follows leaves each pod where it
    /// is, so nothing more comes. Had the report's hold run out before the
    /// primary pod packet, the pods would read swapped for a moment, which
    /// plays nothing, as the left pod is still out.
    #[test]
    fn a_role_switch_pauses_once() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        assert_eq!(ids(&action.pause), [":1.1"]);
        let action = change(&mut pause, (InEar, Out), true, &[paused("1")], now);
        assert_eq!(action, Action::default(), "swapped for a moment");
        let action = change(&mut pause, (Out, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default(), "read right");
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(ids(&action.play), [":1.1"]);
    }

    /// Forgotten, as when the setting is turned off or the link goes,
    /// nothing plays when the pods are back.
    #[test]
    fn forgotten_plays_nothing() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        pause.forget();
        assert!(!pause.is_paused());
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default());
    }

    /// A player that refused `Pause` is not paused by it: it is not played
    /// again. One left still plays; none left, nothing is remembered.
    #[test]
    fn a_player_that_refused_the_pause_is_not_played() {
        let now = Instant::now();
        let mut pause = at(InEar, InEar);
        let action = change(
            &mut pause,
            (Out, InEar),
            true,
            &[playing("1"), playing("2")],
            now,
        );
        assert_eq!(ids(&action.pause), [":1.1", ":1.2"]);
        pause.not_paused(&[":1.1".to_string()]);
        assert!(pause.is_paused());
        let action = change(
            &mut pause,
            (InEar, InEar),
            true,
            &[paused("1"), paused("2")],
            now + PAUSE_SETTLE,
        );
        assert_eq!(ids(&action.play), [":1.2"]);

        let mut pause = at(InEar, InEar);
        change(&mut pause, (Out, InEar), true, &[playing("1")], now);
        pause.not_paused(&[":1.1".to_string()]);
        assert!(!pause.is_paused(), "none left");
        let action = change(&mut pause, (InEar, InEar), true, &[paused("1")], now);
        assert_eq!(action, Action::default());
    }

    /// Turned off from on counts, and turned on does not: off and on again
    /// reads on, but not as it was.
    #[test]
    fn the_switch_counts_each_time_it_is_turned_off() {
        let mut switch = Switch::new(true);
        switch.set(true);
        assert_eq!(switch, Switch::new(true));
        switch.set(false);
        switch.set(false);
        switch.set(true);
        assert_eq!(switch, Switch { on: true, offs: 1 });
        let mut saved_off = Switch::new(false);
        saved_off.set(true);
        assert_eq!(saved_off.offs, 0, "on from off is no turning off");
    }

    #[test]
    fn reads_mpris_statuses() {
        for status in [Playback::Playing, Playback::Paused, Playback::Stopped] {
            assert_eq!(Playback::parse(status.name()), Some(status));
        }
        assert_eq!(Playback::parse("playing"), None);
        assert_eq!(Playback::parse(""), None);
    }
}
