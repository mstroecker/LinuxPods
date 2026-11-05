//! Lowering the media's volume while the user speaks: the host's half of
//! Conversation Awareness (#150). The pair's own A2DP volume -
//! `org.bluez.MediaTransport1.Volume`, its AVRCP absolute volume - goes down
//! at once at the first `01`/`02` of a stretch, and fades back from the first
//! level of `03` or above; only while A2DP plays, never what the user moved,
//! put back when the link goes, at quit, and once a transport that went is
//! back. `docs/app/volume.md` says how and why, and where each
//! reading comes from.
//!
//! [`Ducking`] decides, from the levels, the transport as it reads and the
//! time, as `noise::ModeState` does, and is tested on its own. [`Volume::run`]
//! carries it out over D-Bus, in a task of its own beside attachment - not in
//! a pair's supervisor, which is aborted wherever it is when the pair
//! detaches. It takes the levels as events from the coordinator,
//! [`Coordinator::speech_events`], rather than from snapshots, which keep only
//! the latest - see [`SpeechEvent`]. It looks the transport up afresh for
//! every decision, never keeps its path, takes no signal, and makes every
//! call without auto-start and within [`CALL_LIMIT`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{mpsc, oneshot};
use zbus::Connection;
use zbus::fdo::ManagedObjects;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::aap::conversation::Speech;
use crate::bluez::BLUEZ_SERVICE;
use crate::podstate::{Coordinator, SpeechEvent};

/// How far a stretch of speech lowers the volume: to this share of the
/// volume before it, in percent. Fixed, as in the #150 prototype; LibrePods
/// lowers to 20% on Linux and 43% by default on Android. A Preferences
/// setting may come later.
pub const LOWERED_PERCENT: u16 = 20;

/// How long the volume takes to come back after a stretch of speech: a
/// choice, #150's plan, not a measure of anything.
pub const FADE: Duration = Duration::from_millis(1500);

/// How often a fade steps the volume up: fifteen steps over [`FADE`]. A
/// choice.
pub const FADE_STEP: Duration = Duration::from_millis(100);

/// How far a reading may be from a volume set this stretch and still count
/// as ours, as the pods' answer may be rounded: 4 of 127. A guess - no pair
/// has been seen rounding (#150).
pub const READ_BACK_SLACK: u16 = 4;

/// How long a transport that went is looked for, from when it went with the
/// link up, or from the pair's next link: a guess at how long A2DP takes to
/// come back; not watched (#150).
pub const PUT_BACK_WINDOW: Duration = Duration::from_secs(30);

/// How often it is looked for meanwhile: a guess, as the window is.
pub const PUT_BACK_CHECK: Duration = Duration::from_secs(1);

/// The longest one call to BlueZ, or making the connection, may take: a
/// guess at what no bluetoothd that answers at all would need. One that
/// takes longer counts as the connection failing, and the next is made on a
/// new one.
pub const CALL_LIMIT: Duration = Duration::from_secs(1);

/// The longest the volume may take to be put back as the app shuts down: a
/// choice, short enough that a quit never seems to hang.
pub const RESTORE_LIMIT: Duration = Duration::from_secs(2);

/// The highest A2DP volume, AVRCP's absolute volume.
pub const MAX_VOLUME: u16 = 127;

/// `org.bluez.MediaTransport1`, on every transport.
const TRANSPORT_INTERFACE: &str = "org.bluez.MediaTransport1";

/// A transport's `UUID` for A2DP's audio source, the host's end of
/// headphones: the one transport there is to lower.
const A2DP_SOURCE_UUID: &str = "0000110a-0000-1000-8000-00805f9b34fb";

/// What [`Ducking`] needs of the pair's A2DP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transport {
    /// Its `Volume`, 0 to [`MAX_VOLUME`].
    pub volume: u16,
    /// Whether a stretch of speech may lower it: it is `active`, streaming
    /// and acquired. Not in a call, as far as is assumed - see the doc. What
    /// `crate::media` takes as audio playing to the pair, too, through
    /// [`a2dp_active_in`].
    pub playing: bool,
}

/// The volume a stretch of speech lowers `before` to: [`LOWERED_PERCENT`]
/// of it, rounded, and 1 at least for a volume that is not 0 - which leaves
/// 1 as it is.
pub fn lowered(before: u16) -> u16 {
    if before == 0 {
        return 0;
    }
    let lowered = (u32::from(before) * u32::from(LOWERED_PERCENT) + 50) / 100;
    u16::try_from(lowered).unwrap_or(before).max(1)
}

/// The volume a fade from `from` to `to` has reached `elapsed` into it.
fn faded(from: u16, to: u16, elapsed: Duration) -> u16 {
    if elapsed >= FADE {
        return to;
    }
    let span = i128::from(to) - i128::from(from);
    let reached = i128::from(from) + span * elapsed.as_nanos() as i128 / FADE.as_nanos() as i128;
    u16::try_from(reached).unwrap_or(to)
}

/// One pair's volume through stretches of speech. Takes the levels, the
/// transport as it reads now and the time, and says what to set the
/// transport to; the caller sets it. What counts as ours is `is_ours`;
/// `docs/app/volume.md` says why.
#[derive(Debug, Clone, Default)]
pub struct Ducking {
    state: State,
    /// The volumes set this stretch, a bit each: 0 to [`MAX_VOLUME`].
    ours: u128,
    /// Whether the lowering is still unanswered: no reading but the volume
    /// before has come since.
    unanswered: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    /// Nothing lowered.
    #[default]
    Idle,
    /// Lowered from `before` to `down`, while the user speaks.
    Lowered { before: u16, down: u16 },
    /// On the way back to `before`, from `from` at `start`, the next step
    /// due at `next`.
    Fading {
        before: u16,
        from: u16,
        start: Instant,
        next: Instant,
    },
    /// The user changed the volume during this stretch: nothing more is
    /// done until it ends.
    Overridden,
    /// Lowered from `before`, and the transport gone, or not yet answering:
    /// looked for at `next`, until `until`; with neither, at the pair's next
    /// link, or the next level.
    Waiting {
        before: u16,
        until: Option<Instant>,
        next: Option<Instant>,
    },
}

impl State {
    /// The volume remembered, while there is one to put back.
    fn before(self) -> Option<u16> {
        match self {
            Self::Lowered { before, .. }
            | Self::Fading { before, .. }
            | Self::Waiting { before, .. } => Some(before),
            Self::Idle | Self::Overridden => None,
        }
    }
}

impl Ducking {
    /// Takes a speech level that came at `now`, with the pair's A2DP
    /// transport as it reads now - `None` without one - and returns the
    /// volume to set it to, if any.
    pub fn level(&mut self, level: u8, transport: Option<Transport>, now: Instant) -> Option<u16> {
        let speech = Speech::of(level)?;
        self.observe(transport);
        match speech {
            Speech::Speaking => self.speaking(transport),
            Speech::Easing | Speech::Normal | Speech::Idle => {
                self.speech_over(transport, now);
                None
            }
        }
    }

    /// Takes a reading: one of another volume than the one before means the
    /// pods answered the lowering.
    fn observe(&mut self, transport: Option<Transport>) {
        if let (Some(t), Some(before)) = (transport, self.state.before())
            && t.volume != before
        {
            self.unanswered = false;
        }
    }

    /// Whether `transport` reads a volume that is ours: within
    /// [`READ_BACK_SLACK`] of one set this stretch, or the volume before
    /// while the lowering is unanswered.
    fn is_ours(&self, transport: Option<Transport>) -> bool {
        let Some(Transport { volume, .. }) = transport else {
            return false;
        };
        if self.unanswered && self.state.before() == Some(volume) {
            return true;
        }
        let low = volume.saturating_sub(READ_BACK_SLACK);
        let high = volume.saturating_add(READ_BACK_SLACK).min(MAX_VOLUME);
        (low..=high).any(|v| self.ours & (1 << v) != 0)
    }

    /// Whether `transport` reads the volume before, the lowering unanswered.
    fn is_unanswered(&self, transport: Transport) -> bool {
        self.unanswered && self.state.before() == Some(transport.volume)
    }

    /// Counts `volume` as ours from here.
    fn own(&mut self, volume: u16) {
        if volume <= MAX_VOLUME {
            self.ours |= 1 << volume;
        }
    }

    /// Nothing lowered any more, and nothing ours.
    fn forget(&mut self) {
        *self = Self::default();
    }

    /// A `01`/`02`: lowered at once, if it is not already.
    fn speaking(&mut self, transport: Option<Transport>) -> Option<u16> {
        match self.state {
            State::Idle => self.lower(transport),
            State::Lowered { .. } => {
                // Gone meanwhile, there is nothing to tell; the end of the
                // stretch waits for it.
                if transport.is_some() && !self.is_ours(transport) {
                    self.forget();
                    self.state = State::Overridden;
                }
                None
            }
            State::Fading { before, .. } | State::Waiting { before, .. } => {
                let transport = transport?;
                if !self.is_ours(Some(transport)) {
                    // The user's since the stretch ended: a new stretch,
                    // lowered from there.
                    self.forget();
                    return self.lower(Some(transport));
                }
                // A call meanwhile, say: the fade goes on, putting back what
                // was lowered.
                if !transport.playing {
                    return None;
                }
                let down = lowered(before);
                self.own(down);
                self.state = State::Lowered { before, down };
                (down != transport.volume).then_some(down)
            }
            State::Overridden => None,
        }
    }

    /// The first `01`/`02` of a stretch, from nothing lowered.
    fn lower(&mut self, transport: Option<Transport>) -> Option<u16> {
        let Transport { volume, playing } = transport?;
        let down = lowered(volume);
        if !playing || down == volume {
            return None;
        }
        self.state = State::Lowered {
            before: volume,
            down,
        };
        self.own(down);
        self.unanswered = true;
        Some(down)
    }

    /// A level of `03` or above: what was lowered fades back from here.
    fn speech_over(&mut self, transport: Option<Transport>, now: Instant) {
        match self.state {
            State::Lowered { before, down } => match transport {
                // From what was set, whether the pods have answered it yet
                // or not.
                Some(t) if self.is_ours(Some(t)) => self.fade(before, down, now),
                Some(_) => self.forget(),
                None => self.wait(before, now),
            },
            State::Waiting { before, .. } => match transport {
                Some(t) if self.is_unanswered(t) => {}
                Some(t) if self.is_ours(Some(t)) => self.fade(before, t.volume, now),
                Some(_) => self.forget(),
                None => {}
            },
            State::Overridden => self.forget(),
            State::Idle | State::Fading { .. } => {}
        }
    }

    /// A fade back to `before` from `from` starts at `now`.
    fn fade(&mut self, before: u16, from: u16, now: Instant) {
        self.state = State::Fading {
            before,
            from,
            start: now,
            next: now + FADE_STEP,
        };
    }

    /// The transport went with the link up: looked for from `now`, for
    /// [`PUT_BACK_WINDOW`].
    fn wait(&mut self, before: u16, now: Instant) {
        self.state = State::Waiting {
            before,
            until: Some(now + PUT_BACK_WINDOW),
            next: Some(now + PUT_BACK_CHECK),
        };
    }

    /// When the next step of a fade, or the next look for a transport that
    /// went, is due.
    pub fn next_due(&self) -> Option<Instant> {
        match self.state {
            State::Fading { next, .. } => Some(next),
            State::Waiting { next, .. } => next,
            _ => None,
        }
    }

    /// Takes a fade's step, or looks for a transport that went, at `now`,
    /// with the transport as it reads now, and returns the volume to set it
    /// to, if any. The last step puts back the volume remembered, and ends
    /// the fade; a transport moved by the user ends it with nothing set, and
    /// one gone is waited for.
    pub fn step(&mut self, transport: Option<Transport>, now: Instant) -> Option<u16> {
        if self.next_due().is_none_or(|due| now < due) {
            return None;
        }
        self.observe(transport);
        match self.state {
            State::Fading {
                before,
                from,
                start,
                next,
            } => {
                let Some(t) = transport else {
                    self.wait(before, now);
                    return None;
                };
                if !self.is_ours(Some(t)) {
                    self.forget();
                    return None;
                }
                let target = faded(from, before, now.saturating_duration_since(start));
                if target == before {
                    self.forget();
                } else {
                    self.own(target);
                    self.state = State::Fading {
                        before,
                        from,
                        start,
                        // A step late, as on a loaded machine, is not caught
                        // up on.
                        next: (next + FADE_STEP).max(now + FADE_STEP / 2),
                    };
                }
                (target != t.volume).then_some(target)
            }
            State::Waiting { before, until, .. } => match transport {
                Some(t) if !self.is_unanswered(t) => {
                    let ours = self.is_ours(Some(t));
                    self.forget();
                    (ours && t.volume != before).then_some(before)
                }
                // Gone, or the lowering not answered yet: looked for again,
                // and once the window is over, at the next link or level.
                _ => {
                    let over = until.is_some_and(|until| now >= until);
                    self.state = State::Waiting {
                        before,
                        until: until.filter(|_| !over),
                        next: (!over).then_some(now + PUT_BACK_CHECK),
                    };
                    None
                }
            },
            _ => None,
        }
    }

    /// The pair's link came up at `now`: a transport that went is looked
    /// for from now, for [`PUT_BACK_WINDOW`].
    pub fn link_up(&mut self, now: Instant) {
        if let State::Waiting { before, .. } = self.state {
            self.state = State::Waiting {
                before,
                until: Some(now + PUT_BACK_WINDOW),
                next: Some(now),
            };
        }
    }

    /// The pair's link went: what was lowered is put back at once, as the
    /// transport reads now. With no transport, or the lowering not answered
    /// yet, it waits for the next link. Returns the volume to set it to, if
    /// any.
    pub fn link_gone(&mut self, transport: Option<Transport>) -> Option<u16> {
        self.observe(transport);
        let Some(before) = self.state.before() else {
            self.forget();
            return None;
        };
        match transport {
            Some(t) if !self.is_unanswered(t) => {
                let ours = self.is_ours(Some(t));
                self.forget();
                (ours && t.volume != before).then_some(before)
            }
            _ => {
                self.state = State::Waiting {
                    before,
                    until: None,
                    next: None,
                };
                None
            }
        }
    }

    /// Puts back what was lowered, at once, for the app shutting down.
    /// Returns the volume to set the transport, as it reads now, to - none
    /// if it was moved by the user, or is gone.
    pub fn restore(&mut self, transport: Option<Transport>) -> Option<u16> {
        self.observe(transport);
        let ours = self.is_ours(transport);
        let before = self.state.before();
        self.forget();
        let (before, t) = (before?, transport?);
        (ours && t.volume != before).then_some(before)
    }

    /// Whether the volume is lowered, on its way back, or waiting to be put
    /// back.
    pub fn is_lowered(&self) -> bool {
        self.state.before().is_some()
    }

    /// Whether the user took the volume over during this stretch.
    fn is_overridden(&self) -> bool {
        self.state == State::Overridden
    }
}

/// The pair's A2DP transport, as BlueZ's objects show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2dp {
    /// Where it is: under the pair's device, new with every A2DP setup.
    pub path: OwnedObjectPath,
    pub transport: Transport,
}

/// A transport of the pair's A2DP among BlueZ's objects: where it is, its
/// `Volume` if it has one, and whether it is `active`.
struct Found {
    path: OwnedObjectPath,
    volume: Option<u16>,
    active: bool,
}

/// The A2DP transports of the pair at `mac` among `objects`, BlueZ's: under
/// the pair's device path, their `Device` that device, their `UUID` A2DP's
/// audio source.
fn a2dp_transports(objects: &ManagedObjects, mac: Address) -> Vec<Found> {
    let devices: Vec<&str> = objects
        .iter()
        .filter(|(_, interfaces)| {
            interfaces
                .get("org.bluez.Device1")
                .and_then(|props| props.get("Address"))
                .and_then(|address| String::try_from(address.clone()).ok())
                .and_then(|address| address.parse::<Address>().ok())
                == Some(mac)
        })
        .map(|(path, _)| path.as_str())
        .collect();
    objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get(TRANSPORT_INTERFACE)?;
            let device = props
                .get("Device")
                .and_then(|device| OwnedObjectPath::try_from(device.clone()).ok())?;
            let under = path
                .as_str()
                .strip_prefix(device.as_str())
                .is_some_and(|rest| rest.starts_with('/'));
            let uuid = props
                .get("UUID")
                .and_then(|uuid| String::try_from(uuid.clone()).ok())?;
            if !under
                || !devices.contains(&device.as_str())
                || !uuid.eq_ignore_ascii_case(A2DP_SOURCE_UUID)
            {
                return None;
            }
            let volume = props
                .get("Volume")
                .and_then(|volume| u16::try_from(volume.clone()).ok());
            let active = props
                .get("State")
                .and_then(|state| String::try_from(state.clone()).ok())
                .is_some_and(|state| state == "active");
            Some(Found {
                path: path.clone(),
                volume,
                active,
            })
        })
        .collect()
}

/// The A2DP transport of the pair at `mac` among `objects`, BlueZ's, if it
/// has one with a volume: under the pair's device path, its `Device` that
/// device, its `UUID` A2DP's audio source. One that is `active` is taken
/// before one that is not.
pub fn a2dp_in(objects: &ManagedObjects, mac: Address) -> Option<A2dp> {
    let mut a2dp: Vec<A2dp> = a2dp_transports(objects, mac)
        .into_iter()
        .filter_map(|found| {
            Some(A2dp {
                path: found.path,
                transport: Transport {
                    volume: found.volume?,
                    playing: found.active,
                },
            })
        })
        .collect();
    // The active one first, then by path, so the pick does not hang on the
    // map's order.
    a2dp.sort_by(|a, b| {
        (!a.transport.playing, a.path.as_str()).cmp(&(!b.transport.playing, b.path.as_str()))
    });
    a2dp.into_iter().next()
}

/// Whether an A2DP transport of the pair at `mac` among `objects`, BlueZ's,
/// is `active` - found as [`a2dp_in`] finds one, with a `Volume` or not:
/// audio plays to the pair, for `crate::media`, which sets no volume.
pub fn a2dp_active_in(objects: &ManagedObjects, mac: Address) -> bool {
    a2dp_transports(objects, mac)
        .iter()
        .any(|found| found.active)
}

/// `call`, unless it takes longer than [`CALL_LIMIT`].
async fn bounded<T>(what: &str, call: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(CALL_LIMIT, call)
        .await
        .with_context(|| format!("BlueZ did not answer within {CALL_LIMIT:?} {what}"))?
}

/// BlueZ's objects, read on `conn` now without auto-start. Unbounded: the
/// caller bounds it.
pub async fn bluez_objects(conn: &Connection) -> Result<ManagedObjects> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ_SERVICE)?
        .path("/")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let objects: Option<ManagedObjects> = manager
        .inner()
        .call_with_flags("GetManagedObjects", MethodFlags::NoAutoStart.into(), &())
        .await
        .context("failed to read BlueZ's objects")?;
    objects.context("BlueZ sent no reply")
}

/// The A2DP transport of the pair at `mac`, read from BlueZ on `conn` now.
/// `None` when it has none.
pub async fn find_a2dp(conn: &Connection, mac: Address) -> Result<Option<A2dp>> {
    bounded("reading its objects", async {
        Ok(a2dp_in(&bluez_objects(conn).await?, mac))
    })
    .await
}

/// Sets the transport at `path` to `volume`. bluetoothd changes the property
/// only once the pods answer - see the doc.
pub async fn set_volume(conn: &Connection, path: &OwnedObjectPath, volume: u16) -> Result<()> {
    bounded("setting the volume", async {
        let properties = zbus::fdo::PropertiesProxy::builder(conn)
            .destination(BLUEZ_SERVICE)?
            .path(path.to_owned())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let _: Option<()> = properties
            .inner()
            .call_with_flags(
                "Set",
                MethodFlags::NoAutoStart.into(),
                &(TRANSPORT_INTERFACE, "Volume", Value::from(volume)),
            )
            .await
            .with_context(|| format!("failed to set the volume of {path}"))?;
        Ok(())
    })
    .await
}

/// Where [`Volume`] reaches BlueZ: a builder for the system bus, or under
/// the harness a private one's. Built when first needed, and again after a
/// connection failed. `crate::media::Media` takes one for BlueZ's bus too,
/// and another for the session bus, where the media players are.
pub type Connect = Box<dyn FnMut() -> zbus::Result<zbus::connection::Builder<'static>> + Send>;

/// What lowers the volume while the user speaks, for every pair with a
/// link. Its task is [`Volume::run`]; [`VolumeControl`] asks it to put
/// everything back.
pub struct Volume {
    events: mpsc::UnboundedReceiver<SpeechEvent>,
    requests: mpsc::UnboundedReceiver<Request>,
    connect: Connect,
    conn: Option<Connection>,
    /// Each pair's, while a stretch of speech has lowered it, is under way,
    /// or waits for its transport.
    pairs: HashMap<Address, Ducking>,
    /// Set once everything was put back for the app to shut down: levels
    /// from then on change nothing.
    closing: bool,
}

/// What [`VolumeControl`] asks of [`Volume::run`].
enum Request {
    /// Put back every volume lowered, then say so.
    RestoreAll(oneshot::Sender<()>),
    /// Say, once every event told so far is taken, whether anything is still
    /// lowered or waits to be put back.
    Settled(oneshot::Sender<bool>),
}

/// Asks [`Volume::run`] to put back every volume it lowered.
#[derive(Clone)]
pub struct VolumeControl {
    requests: mpsc::UnboundedSender<Request>,
}

impl VolumeControl {
    /// Puts back every volume lowered for speech, and changes none from here
    /// on: for the app shutting down. Back once it is done, or the task is
    /// gone; the caller bounds the wait, as with [`RESTORE_LIMIT`].
    pub async fn restore_all(&self) {
        let (done, finished) = oneshot::channel();
        if self.requests.send(Request::RestoreAll(done)).is_ok() {
            let _ = finished.await;
        }
    }

    /// Whether nothing is lowered, nor on its way back, nor waiting to be
    /// put back, once every speech event the coordinator told before this
    /// call has been taken: nothing more will be set until the next event.
    /// For a test to see that nothing comes, rather than wait and see.
    /// `false` once the task is gone.
    pub async fn settled(&self) -> bool {
        let (done, answer) = oneshot::channel();
        if self.requests.send(Request::Settled(done)).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }
}

impl Volume {
    /// Lowers the volume on `coordinator`'s speech levels, from now on,
    /// reaching BlueZ through `connect`.
    pub fn new(coordinator: &Coordinator, connect: Connect) -> (Self, VolumeControl) {
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let volume = Self {
            events: coordinator.speech_events(),
            requests,
            connect,
            conn: None,
            pairs: HashMap::new(),
            closing: false,
        };
        (
            volume,
            VolumeControl {
                requests: requests_tx,
            },
        )
    }

    /// Takes the levels and fades the volumes until the coordinator goes.
    /// Never ends otherwise: BlueZ failing to answer costs a level or a step,
    /// and the connection is made again at the next.
    ///
    /// Events come first, then requests: a request is taken only with no
    /// event waiting, so [`VolumeControl::settled`] answers for every event
    /// told before it.
    pub async fn run(mut self) {
        let mut requests_open = true;
        loop {
            let next = self.pairs.values().filter_map(Ducking::next_due).min();
            let due = async move {
                match next {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                event = self.events.recv() => match event {
                    Some(event) => self.handle(event).await,
                    None => return,
                },
                request = self.requests.recv(), if requests_open => match request {
                    Some(Request::RestoreAll(done)) => {
                        self.restore_all().await;
                        let _ = done.send(());
                    }
                    Some(Request::Settled(answer)) => {
                        let settled = self.pairs.values().all(|d| !d.is_lowered());
                        let _ = answer.send(settled);
                    }
                    None => requests_open = false,
                },
                () = due => self.steps(Instant::now()).await,
            }
        }
    }

    async fn handle(&mut self, event: SpeechEvent) {
        match event {
            SpeechEvent::Level { mac, level } => {
                if self.closing {
                    return;
                }
                // Read only once a level may set something: an 08 or 09
                // while nothing is lowered - around every mode change -
                // costs no call.
                let ducking = self.pairs.entry(mac).or_default();
                let speaking = Speech::of(level) == Some(Speech::Speaking);
                if !speaking && !ducking.is_lowered() {
                    // Ends a stretch the user took over, which needs no
                    // transport.
                    ducking.level(level, None, Instant::now());
                    self.forget_idle(mac);
                    return;
                }
                let found = match self.a2dp(mac).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::debug!("speech level of {mac} not acted on: {e:#}");
                        return;
                    }
                };
                let ducking = self.pairs.entry(mac).or_default();
                let was_lowered = ducking.is_lowered();
                let set = ducking.level(level, found.as_ref().map(|a| a.transport), Instant::now());
                if let (Some(volume), Some(a2dp)) = (set, &found) {
                    if !was_lowered {
                        tracing::info!(
                            "Lowering the volume of {mac} to {volume} of {MAX_VOLUME}, from {}, \
                             while the user speaks",
                            a2dp.transport.volume
                        );
                    }
                    self.set(mac, a2dp, volume).await;
                }
                self.forget_idle(mac);
            }
            SpeechEvent::LinkUp { mac } => {
                if let Some(ducking) = self.pairs.get_mut(&mac) {
                    ducking.link_up(Instant::now());
                }
            }
            SpeechEvent::LinkGone { mac } => {
                if !self.pairs.get(&mac).is_some_and(Ducking::is_lowered) {
                    self.pairs.remove(&mac);
                    return;
                }
                // Unread, the transport counts as gone, and is waited for.
                let found = self.a2dp(mac).await.unwrap_or_else(|e| {
                    tracing::debug!("volume of {mac} not put back yet: {e:#}");
                    None
                });
                let Some(ducking) = self.pairs.get_mut(&mac) else {
                    return;
                };
                let set = ducking.link_gone(found.as_ref().map(|a| a.transport));
                if let (Some(volume), Some(a2dp)) = (set, &found) {
                    tracing::info!("Volume of {mac} put back at {volume}: its link went");
                    self.set(mac, a2dp, volume).await;
                } else if ducking.is_lowered() {
                    tracing::info!(
                        "Volume of {mac} to be put back at its next link: its link went"
                    );
                }
                self.forget_idle(mac);
            }
        }
    }

    /// Takes every fade's step, and every look for a transport that went,
    /// that is due by `now`.
    async fn steps(&mut self, now: Instant) {
        let due: Vec<Address> = self
            .pairs
            .iter()
            .filter(|(_, ducking)| ducking.next_due().is_some_and(|at| at <= now))
            .map(|(&mac, _)| mac)
            .collect();
        for mac in due {
            // Unread, the transport counts as gone, which waits for it: a
            // step left due would be retried at once, in a loop.
            let found = self.a2dp(mac).await.unwrap_or_else(|e| {
                tracing::debug!("volume of {mac} not read: {e:#}");
                None
            });
            let Some(ducking) = self.pairs.get_mut(&mac) else {
                continue;
            };
            let set = ducking.step(found.as_ref().map(|a| a.transport), Instant::now());
            let done = !ducking.is_lowered();
            if let (Some(volume), Some(a2dp)) = (set, &found) {
                self.set(mac, a2dp, volume).await;
                if done {
                    tracing::info!("Volume of {mac} back at {volume} after the user spoke");
                }
            } else if done {
                tracing::info!(
                    "Volume of {mac} left where it is: changed meanwhile, or its transport \
                     went and came back at another volume"
                );
            }
            self.forget_idle(mac);
        }
    }

    /// Puts back every volume lowered, and acts on no level from here on.
    /// One whose transport went cannot be put back now.
    async fn restore_all(&mut self) {
        self.closing = true;
        let pairs: Vec<(Address, Ducking)> = self.pairs.drain().collect();
        for (mac, mut ducking) in pairs {
            if !ducking.is_lowered() {
                continue;
            }
            let found = match self.a2dp(mac).await {
                Ok(found) => found,
                Err(e) => {
                    tracing::warn!("failed to put back the volume of {mac}: {e:#}");
                    continue;
                }
            };
            let set = ducking.restore(found.as_ref().map(|a| a.transport));
            if let (Some(volume), Some(a2dp)) = (set, &found) {
                tracing::info!("Volume of {mac} put back at {volume}: the app is quitting");
                self.set(mac, a2dp, volume).await;
            }
        }
    }

    /// Drops the pair's entry once nothing is lowered.
    fn forget_idle(&mut self, mac: Address) {
        if self
            .pairs
            .get(&mac)
            .is_some_and(|ducking| !ducking.is_lowered() && !ducking.is_overridden())
        {
            self.pairs.remove(&mac);
        }
    }

    /// The pair's A2DP transport, read now.
    async fn a2dp(&mut self, mac: Address) -> Result<Option<A2dp>> {
        let conn = self.connection().await?;
        let found = find_a2dp(&conn, mac).await;
        self.check(&found);
        found
    }

    /// Drops the connection after a failure that was not BlueZ's answer: it
    /// is made again next time.
    fn check<T>(&mut self, result: &Result<T>) {
        if let Err(e) = result
            && !is_bluez_error(e)
        {
            self.conn = None;
        }
    }

    /// Sets the transport, logging a failure: the fade's next step, or the
    /// next level, reads the transport again.
    async fn set(&mut self, mac: Address, a2dp: &A2dp, volume: u16) {
        let Ok(conn) = self.connection().await else {
            return;
        };
        let set = set_volume(&conn, &a2dp.path, volume).await;
        self.check(&set);
        match set {
            // The reading is the pods' last answer, or the user's: a rounded
            // answer shows as the next set's reading.
            Ok(()) => tracing::debug!(
                "volume of {mac} set to {volume} at {}, reading {}",
                a2dp.path,
                a2dp.transport.volume
            ),
            Err(e) => tracing::warn!("{e:#}"),
        }
    }

    /// The connection to BlueZ's bus, made if there is none, within
    /// [`CALL_LIMIT`].
    async fn connection(&mut self) -> Result<Connection> {
        if let Some(conn) = &self.conn {
            return Ok(conn.clone());
        }
        let builder = (self.connect)().context("failed to find the bus BlueZ is on")?;
        let conn = tokio::time::timeout(CALL_LIMIT, builder.build())
            .await
            .with_context(|| format!("no connection to the bus BlueZ is on in {CALL_LIMIT:?}"))?
            .context("failed to connect to the bus BlueZ is on")?;
        self.conn = Some(conn.clone());
        Ok(conn)
    }
}

/// Whether `e` is an answer - an error BlueZ, or the bus for it, sent back -
/// rather than the connection failing or taking too long.
fn is_bluez_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<zbus::Error>(),
            Some(zbus::Error::MethodError(..) | zbus::Error::FDO(_))
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, OwnedValue};

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

    fn playing(volume: u16) -> Option<Transport> {
        Some(Transport {
            volume,
            playing: true,
        })
    }

    fn idle(volume: u16) -> Option<Transport> {
        Some(Transport {
            volume,
            playing: false,
        })
    }

    /// Runs a fade to its end, the transport following each step at once,
    /// and returns the volumes set and when, from the fade's start.
    fn fade(ducking: &mut Ducking, mut volume: u16) -> Vec<(Duration, u16)> {
        let start = ducking.next_due().unwrap() - FADE_STEP;
        let mut set = Vec::new();
        while let Some(at) = ducking.next_due() {
            if let Some(v) = ducking.step(playing(volume), at) {
                volume = v;
                set.push((at - start, v));
            }
        }
        set
    }

    #[test]
    fn lowers_to_a_fifth() {
        assert_eq!(lowered(100), 20);
        assert_eq!(lowered(32), 6);
        assert_eq!(lowered(127), 25);
        assert_eq!(lowered(3), 1);
        assert_eq!(lowered(2), 1, "never to silence");
        assert_eq!(lowered(1), 1);
        assert_eq!(lowered(0), 0);
    }

    /// The #150 run's first stretch: down at the first 01, nothing at 02,
    /// back up in steps from 03 over 1.5 s whatever comes meanwhile, to
    /// exactly the volume before.
    #[test]
    fn down_at_once_and_up_slowly() {
        let start = Instant::now();
        let mut ducking = Ducking::default();
        assert_eq!(ducking.level(0x09, playing(100), start), None);
        assert_eq!(ducking.level(0x01, playing(100), start), Some(20));
        assert!(ducking.is_lowered());
        assert_eq!(ducking.level(0x02, playing(20), start), None);
        assert_eq!(ducking.next_due(), None, "no fade while speaking");

        let end = start + Duration::from_secs(6);
        assert_eq!(ducking.level(0x03, playing(20), end), None);
        assert_eq!(ducking.next_due(), Some(end + FADE_STEP));
        for level in [0x07, 0x08, 0x09] {
            assert_eq!(ducking.level(level, playing(20), end), None);
        }
        let steps = fade(&mut ducking, 20);
        assert_eq!(steps.len(), 15, "{steps:?}");
        assert_eq!(steps.first(), Some(&(FADE_STEP, 25)));
        assert_eq!(steps.last(), Some(&(FADE, 100)));
        assert!(steps.windows(2).all(|w| w[0].1 < w[1].1), "{steps:?}");
        assert!(!ducking.is_lowered());
    }

    /// The pods slow to answer the lowering: the transport still reads the
    /// volume before at 02 and at 03, which is ours while unanswered. The
    /// stretch stays lowered, and the fade starts from the volume set, so a
    /// late answer is faded back from too.
    #[test]
    fn a_lowering_not_yet_answered_is_ours() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        assert_eq!(ducking.level(0x01, playing(100), now), Some(20));
        assert_eq!(ducking.level(0x02, playing(100), now), None);
        assert!(ducking.is_lowered(), "never taken for the user's");
        ducking.level(0x03, playing(100), now);
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(100), at), Some(25), "from 20");
        // Answered late: 20, then the step's 25 rounded down.
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(20), at), Some(30));
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(24), at), Some(36));
        // Answered, the volume before is no longer ours mid-fade: the user's.
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(100), at), None);
        assert!(!ducking.is_lowered());
    }

    /// A late and rounded answer to a fade step goes on with the fade; one
    /// further off than the slack is the user's.
    #[test]
    fn a_late_rounded_answer_mid_fade_is_ours() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(20), at), Some(25));
        // Not answered yet: 20 still, which was ours.
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(20), at), Some(30));
        // Answered as 24, rounded.
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(24), at), Some(36));
        assert!(ducking.is_lowered());
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(36 + READ_BACK_SLACK + 1), at), None);
        assert!(!ducking.is_lowered());
    }

    /// The slack's cost: a user change of no more than [`READ_BACK_SLACK`]
    /// from a volume set this stretch is taken for the app's.
    #[test]
    fn a_user_change_within_the_slack_counts_as_ours() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        let near = 20 + READ_BACK_SLACK;
        assert_eq!(ducking.level(0x02, playing(near), now), None);
        assert!(ducking.is_lowered());
        assert_eq!(ducking.level(0x02, playing(near + 1), now), None);
        assert!(!ducking.is_lowered(), "the user's");
    }

    /// Quitting before the pods answered the lowering: the transport still
    /// reads the volume before, which a set would not change, so nothing is
    /// set.
    #[test]
    fn quitting_before_the_lowering_is_answered_sets_nothing() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.restore(playing(100)), None);
        assert!(!ducking.is_lowered());
    }

    /// Speech again while the volume comes back: down at once, to a fifth
    /// of the volume before the first stretch, not of the faded one.
    #[test]
    fn speech_during_the_fade_drops_it_again() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let at = ducking.next_due().unwrap() + FADE_STEP * 4;
        let up = ducking.step(playing(20), at).unwrap();
        assert!(up > 20);
        assert_eq!(ducking.level(0x01, playing(up), at), Some(20));
        assert_eq!(ducking.next_due(), None);
        ducking.level(0x08, playing(20), at);
        assert_eq!(fade(&mut ducking, 20).last().unwrap().1, 100);
    }

    /// Moved by the user while lowered, the volume is theirs: the rest of the
    /// stretch, and its end, set nothing. The next stretch is lowered from
    /// where the user left it.
    #[test]
    fn a_volume_the_user_changed_is_left_alone() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.level(0x02, playing(50), now), None);
        assert_eq!(ducking.level(0x01, playing(50), now), None, "same stretch");
        assert_eq!(ducking.level(0x03, playing(50), now), None);
        assert_eq!(ducking.next_due(), None);
        assert!(!ducking.is_lowered());
        assert_eq!(ducking.level(0x01, playing(50), now), Some(10));

        // Moved at the end of a stretch, before 02 came.
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.level(0x03, playing(40), now), None);
        assert_eq!(ducking.next_due(), None);
        assert_eq!(ducking.restore(playing(40)), None);
    }

    /// Moved by the user during the fade, it ends there; and speech after
    /// that is a new stretch, lowered from the user's volume.
    #[test]
    fn a_change_during_the_fade_ends_it() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let at = ducking.next_due().unwrap();
        let up = ducking.step(playing(20), at).unwrap();
        assert_eq!(ducking.step(playing(up + 30), at + FADE_STEP), None);
        assert!(!ducking.is_lowered());

        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        assert_eq!(ducking.level(0x01, playing(70), now), Some(14));
        ducking.level(0x03, playing(14), now);
        assert_eq!(fade(&mut ducking, 14).last().unwrap().1, 70);
    }

    /// Nothing playing, a call, no transport at all, or nothing to lower:
    /// speech lowers nothing, and the levels around it set nothing either.
    #[test]
    fn nothing_is_lowered_unless_a2dp_plays() {
        let now = Instant::now();
        for transport in [None, idle(100), playing(0), playing(1)] {
            let mut ducking = Ducking::default();
            for level in [0x01, 0x02, 0x03, 0x07, 0x08, 0x09] {
                assert_eq!(ducking.level(level, transport, now), None, "{transport:?}");
            }
            assert!(!ducking.is_lowered());
            assert_eq!(ducking.next_due(), None);
        }
    }

    /// A call beginning while lowered: the volume still comes back, and
    /// speech during the fade no longer lowers it.
    #[test]
    fn a_call_meanwhile_still_gets_the_volume_back() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, idle(20), now);
        let at = ducking.next_due().unwrap();
        let up = ducking.step(idle(20), at).unwrap();
        assert_eq!(ducking.level(0x01, idle(up), at), None);
        assert_eq!(fade(&mut ducking, up).last().unwrap().1, 100);
    }

    /// The link going, or the app quitting, puts the volume back at once,
    /// lowered or halfway up - unless the user moved it.
    #[test]
    fn the_link_going_or_quitting_puts_back_what_was_lowered() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.link_gone(playing(20)), Some(100));
        assert!(!ducking.is_lowered());
        assert_eq!(ducking.link_gone(playing(20)), None, "once");

        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let at = ducking.next_due().unwrap();
        let up = ducking.step(playing(20), at).unwrap();
        assert_eq!(ducking.restore(idle(up)), Some(100));

        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.link_gone(playing(55)), None);
        assert!(!ducking.is_lowered());
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.restore(None), None, "nothing to put back on");
        assert!(!ducking.is_lowered());
    }

    /// The link going before the pods answered the lowering: the transport
    /// reads the volume before, and the answer may land after. It waits for
    /// the next link, and puts the volume back if it reads ours then.
    #[test]
    fn a_lowering_unanswered_when_the_link_went_is_looked_at_again() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.link_gone(playing(100)), None);
        assert!(ducking.is_lowered());
        ducking.link_up(now);
        assert_eq!(ducking.step(playing(20), now), Some(100));
        assert!(!ducking.is_lowered());
    }

    /// The link went with the transport: what was lowered is kept until the
    /// next link, and put back once a transport for the pair reads a volume
    /// that is ours - as bluetoothd starts the next one at the device's
    /// volume. Past the window from the link, it waits for the next link
    /// again.
    #[test]
    fn a_transport_gone_with_the_link_is_put_back_on_the_next() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.link_gone(None), None);
        assert!(ducking.is_lowered());
        assert_eq!(ducking.next_due(), None, "waits for the next link");

        let later = now + Duration::from_secs(3600);
        ducking.link_up(later);
        assert_eq!(ducking.next_due(), Some(later));
        assert_eq!(ducking.step(None, later), None, "A2DP not set up yet");
        let at = ducking.next_due().unwrap();
        assert_eq!(at, later + PUT_BACK_CHECK);
        assert_eq!(ducking.step(idle(20), at), Some(100));
        assert!(!ducking.is_lowered());

        // At the user's volume, it is theirs.
        ducking.level(0x01, playing(100), now);
        ducking.link_gone(None);
        ducking.link_up(later);
        assert_eq!(ducking.step(playing(60), later), None);
        assert!(!ducking.is_lowered());

        // None within the window: kept for the next link.
        ducking.level(0x01, playing(100), now);
        ducking.link_gone(None);
        ducking.link_up(later);
        let mut at = later;
        while let Some(due) = ducking.next_due() {
            at = due;
            assert_eq!(ducking.step(None, due), None);
        }
        assert!(at >= later + PUT_BACK_WINDOW);
        assert!(ducking.is_lowered(), "bluetoothd still holds it lowered");
        let next_link = later + Duration::from_secs(3600);
        ducking.link_up(next_link);
        assert_eq!(ducking.step(playing(20), next_link), Some(100));
    }

    /// The transport gone with the link up - A2DP set up again - is looked
    /// for each second, and put back once it reads ours.
    #[test]
    fn a_transport_gone_with_the_link_up_is_looked_for() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.level(0x02, None, now), None);
        assert!(ducking.is_lowered(), "the stretch goes on");
        ducking.level(0x03, None, now);
        assert_eq!(ducking.next_due(), Some(now + PUT_BACK_CHECK));
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(20), at), Some(100));

        // Gone in the fade, it is looked for the same way.
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let at = ducking.next_due().unwrap();
        let up = ducking.step(playing(20), at).unwrap();
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(None, at), None);
        let at = ducking.next_due().unwrap();
        assert_eq!(ducking.step(playing(up), at), Some(100));
    }

    /// Levels 08 and 09 frame every mode change with the setting on, and an
    /// unknown level means nothing: neither lowers or changes anything.
    #[test]
    fn levels_framing_a_mode_change_set_nothing() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        for level in [0x08, 0x09, 0x00, 0x0C] {
            assert_eq!(ducking.level(level, playing(100), now), None);
        }
        assert!(!ducking.is_lowered());
        ducking.level(0x01, playing(100), now);
        assert_eq!(ducking.level(0x0C, playing(20), now), None);
        assert!(ducking.is_lowered(), "an unknown level ends nothing");
    }

    /// A fade step that comes late takes the volume where it should be by
    /// then, and the next is not crowded behind it.
    #[test]
    fn a_late_step_catches_up() {
        let now = Instant::now();
        let mut ducking = Ducking::default();
        ducking.level(0x01, playing(100), now);
        ducking.level(0x03, playing(20), now);
        let late = now + FADE / 2;
        assert_eq!(ducking.step(playing(20), late), Some(60));
        assert!(ducking.next_due().unwrap() >= late + FADE_STEP / 2);
        assert_eq!(ducking.step(playing(60), now + FADE * 2), Some(100));
        assert!(!ducking.is_lowered());
    }

    fn value<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
        v.into().try_into_owned().unwrap()
    }

    /// BlueZ's objects: the pairs' devices on `hci0`, and `transports`, each
    /// a path, its device, UUID, state and volume.
    fn objects(transports: &[(&str, Address, &str, &str, Option<u16>)]) -> ManagedObjects {
        let device_path =
            |mac: Address| format!("/org/bluez/hci0/dev_{}", mac.to_string().replace(':', "_"));
        let mut objects = ManagedObjects::new();
        for mac in [PAIR, OTHER] {
            let props = HashMap::from([("Address".to_string(), value(mac.to_string()))]);
            objects.insert(
                OwnedObjectPath::try_from(device_path(mac)).unwrap(),
                HashMap::from([("org.bluez.Device1".try_into().unwrap(), props)]),
            );
        }
        for &(path, mac, uuid, state, volume) in transports {
            let device = device_path(mac);
            let path = format!("{device}{path}");
            let mut props = HashMap::from([
                (
                    "Device".to_string(),
                    value(ObjectPath::try_from(device.as_str()).unwrap()),
                ),
                ("UUID".to_string(), value(uuid)),
                ("State".to_string(), value(state)),
            ]);
            if let Some(volume) = volume {
                props.insert("Volume".to_string(), value(volume));
            }
            objects.insert(
                OwnedObjectPath::try_from(path).unwrap(),
                HashMap::from([(TRANSPORT_INTERFACE.try_into().unwrap(), props)]),
            );
        }
        objects
    }

    const SOURCE: &str = "0000110A-0000-1000-8000-00805F9B34FB";
    /// A2DP's audio sink, the end a host takes as a speaker, never for
    /// AirPods.
    const SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

    /// The pair's own A2DP source transport, under its device, whatever its
    /// path; another pair's is not it, nor a sink, and one with no volume is
    /// none.
    #[test]
    fn finds_the_pairs_own_a2dp_transport() {
        let found = a2dp_in(
            &objects(&[
                ("/sep1/fd3", PAIR, SOURCE, "active", Some(64)),
                ("/sep1/fd0", OTHER, SOURCE, "active", Some(90)),
            ]),
            PAIR,
        )
        .unwrap();
        assert!(
            found
                .path
                .as_str()
                .ends_with("dev_00_00_5E_00_53_01/sep1/fd3")
        );
        assert_eq!(found.transport, playing(64).unwrap());

        assert_eq!(a2dp_in(&objects(&[]), PAIR), None, "nothing plays");
        let no_volume = objects(&[("/sep1/fd0", PAIR, SOURCE, "active", None)]);
        assert_eq!(a2dp_in(&no_volume, PAIR), None);
        let other = objects(&[("/sep1/fd0", OTHER, SOURCE, "active", Some(90))]);
        assert_eq!(a2dp_in(&other, PAIR), None);
        let sink = objects(&[("/sep1/fd0", PAIR, SINK, "active", Some(90))]);
        assert_eq!(a2dp_in(&sink, PAIR), None);
    }

    /// Idle or pending, A2DP is there but does not play.
    #[test]
    fn a2dp_plays_only_while_active() {
        for state in ["idle", "pending"] {
            let found = a2dp_in(
                &objects(&[("/sep1/fd0", PAIR, SOURCE, state, Some(64))]),
                PAIR,
            );
            assert_eq!(found.unwrap().transport, idle(64).unwrap(), "{state}");
        }
    }

    /// Pausing the media needs no volume: an active transport without one
    /// is audio playing to the pair all the same, and an idle one, another
    /// pair's or a sink is not.
    #[test]
    fn an_active_transport_needs_no_volume_to_be_playing() {
        let active = |transports: &[(&str, Address, &str, &str, Option<u16>)]| {
            a2dp_active_in(&objects(transports), PAIR)
        };
        assert!(active(&[("/sep1/fd0", PAIR, SOURCE, "active", None)]));
        assert!(active(&[("/sep1/fd0", PAIR, SOURCE, "active", Some(64))]));
        assert!(active(&[
            ("/sep1/fd0", PAIR, SOURCE, "idle", Some(30)),
            ("/sep2/fd1", PAIR, SOURCE, "active", None),
        ]));
        assert!(!active(&[]));
        assert!(!active(&[("/sep1/fd0", PAIR, SOURCE, "idle", None)]));
        assert!(!active(&[("/sep1/fd0", OTHER, SOURCE, "active", None)]));
        assert!(!active(&[("/sep1/fd0", PAIR, SINK, "active", None)]));
    }

    /// Of two A2DP transports, the active one.
    #[test]
    fn the_active_transport_is_taken_first() {
        let found = a2dp_in(
            &objects(&[
                ("/sep1/fd0", PAIR, SOURCE, "idle", Some(30)),
                ("/sep2/fd1", PAIR, SOURCE, "active", Some(64)),
            ]),
            PAIR,
        )
        .unwrap();
        assert!(found.path.as_str().ends_with("/sep2/fd1"));
    }
}
