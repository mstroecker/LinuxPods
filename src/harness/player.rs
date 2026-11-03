//! A fake media player (#167): an MPRIS player on a session bus of the
//! harness's, for the app's `crate::media` to find, pause and play, out of
//! reach of the user's own players and session bus.
//!
//! It holds `org.mpris.MediaPlayer2.<name>` and serves, at
//! `/org/mpris/MediaPlayer2`, as much of MPRIS as the app uses and a player
//! is bound to have: `org.mpris.MediaPlayer2` with its `Identity`, and
//! `org.mpris.MediaPlayer2.Player` with `PlaybackStatus`, `CanPlay`,
//! `CanPause` and `CanControl`, and `Play`, `Pause`, `PlayPause` and `Stop`,
//! which change the status as the MPRIS specification says and signal it.
//! A player starts `Stopped`.
//!
//! What the user does in the player's own window - play, pause, stop - goes
//! through [`FakePlayer::set_status`], in the same process, or over D-Bus
//! through `io.github.mstroecker.LinuxPods.FakePlayer1` beside the rest, for
//! `fakepods` and `scripts/fake-world.sh player`:
//!
//! | Method | Does |
//! |---|---|
//! | `SetStatus(s status)` | `Playing`, `Paused` or `Stopped`, as the user sets it in the player: not among `Calls` |
//! | `Status() → s` | its `PlaybackStatus` |
//! | `Calls() → as` | every MPRIS method called on the player - `Play`, `Pause`, `PlayPause`, `Stop` - in order |
//!
//! A status MPRIS has no name for is `org.freedesktop.DBus.Error.InvalidArgs`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::media::{MPRIS_PATH, MPRIS_PREFIX, Playback};

/// The interface the fake's own controls are on.
pub const CONTROL_INTERFACE: &str = "io.github.mstroecker.LinuxPods.FakePlayer1";

/// A fake MPRIS player. Clones share it; it stays on the bus until
/// [`FakePlayer::close`], or until the last clone is dropped.
#[derive(Clone)]
pub struct FakePlayer {
    conn: Connection,
    bus_name: String,
    state: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    status: Playback,
    /// Every MPRIS method called on the player, in order.
    calls: Vec<String>,
}

impl FakePlayer {
    /// Serves a player called `org.mpris.MediaPlayer2.<name>` on the bus at
    /// `bus`, `Stopped`. Its objects are served before it takes its name,
    /// so nothing finds it half there.
    pub async fn serve(bus: &str, name: &str) -> Result<Self> {
        let bus_name = format!("{MPRIS_PREFIX}{name}");
        let state = Arc::new(Mutex::new(State {
            status: Playback::Stopped,
            calls: Vec::new(),
        }));
        let conn = zbus::connection::Builder::address(bus)?
            .serve_at(
                MPRIS_PATH,
                Root {
                    identity: format!("{name} (fake)"),
                },
            )?
            .serve_at(
                MPRIS_PATH,
                Player {
                    state: state.clone(),
                },
            )?
            .serve_at(
                MPRIS_PATH,
                FakePlayer1 {
                    state: state.clone(),
                },
            )?
            .name(bus_name.clone())?
            .build()
            .await
            .with_context(|| format!("failed to serve {bus_name} on {bus}"))?;
        tracing::info!("Fake player {bus_name} on {bus}");
        Ok(Self {
            conn,
            bus_name,
            state,
        })
    }

    /// The name it holds, `org.mpris.MediaPlayer2.<name>`.
    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }

    /// Its connection's unique name, which the app calls it at.
    pub fn unique_name(&self) -> String {
        self.conn
            .unique_name()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// Its `PlaybackStatus`.
    pub fn status(&self) -> Playback {
        lock(&self.state).status
    }

    /// Every MPRIS method called on it, in order.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    /// Sets its status as the user does in the player's own window, and
    /// signals it.
    pub async fn set_status(&self, status: Playback) -> Result<()> {
        lock(&self.state).status = status;
        let iface = self
            .conn
            .object_server()
            .interface::<_, Player>(MPRIS_PATH)
            .await?;
        iface
            .get()
            .await
            .playback_status_changed(iface.signal_emitter())
            .await?;
        Ok(())
    }

    /// Quits, as a player closed by the user: its name goes, and nothing
    /// answers for it any more.
    pub async fn close(self) -> Result<()> {
        self.conn.release_name(self.bus_name.as_str()).await?;
        self.conn.close().await?;
        Ok(())
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `org.mpris.MediaPlayer2`, the part every player serves.
struct Root {
    identity: String,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn identity(&self) -> String {
        self.identity.clone()
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player`, as far as the app uses it.
struct Player {
    state: Arc<Mutex<State>>,
}

impl Player {
    /// Records `method`, changes the status as `change` says, and signals
    /// it if it changed.
    async fn called(
        &self,
        method: &str,
        emitter: &SignalEmitter<'_>,
        change: impl FnOnce(Playback) -> Playback,
    ) -> fdo::Result<()> {
        let changed = {
            let mut state = lock(&self.state);
            state.calls.push(method.to_string());
            let before = state.status;
            state.status = change(before);
            state.status != before
        };
        if changed {
            self.playback_status_changed(emitter).await?;
        }
        Ok(())
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    /// Starts or resumes playback.
    async fn play(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
        self.called("Play", &emitter, |_| Playback::Playing).await
    }

    /// Pauses playback; a player that is not playing stays as it is.
    async fn pause(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
        self.called("Pause", &emitter, |status| match status {
            Playback::Playing => Playback::Paused,
            other => other,
        })
        .await
    }

    /// Pauses a playing player, and plays any other.
    async fn play_pause(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.called("PlayPause", &emitter, |status| match status {
            Playback::Playing => Playback::Paused,
            _ => Playback::Playing,
        })
        .await
    }

    async fn stop(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
        self.called("Stop", &emitter, |_| Playback::Stopped).await
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        lock(&self.state).status.name().to_string()
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// The fake's own controls - see the module docs.
struct FakePlayer1 {
    state: Arc<Mutex<State>>,
}

#[interface(name = "io.github.mstroecker.LinuxPods.FakePlayer1")]
impl FakePlayer1 {
    async fn set_status(
        &self,
        status: &str,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<()> {
        let status = Playback::parse(status).ok_or_else(|| {
            fdo::Error::InvalidArgs(format!("no status {status:?}: Playing, Paused or Stopped"))
        })?;
        lock(&self.state).status = status;
        let iface = server.interface::<_, Player>(MPRIS_PATH).await?;
        iface
            .get()
            .await
            .playback_status_changed(iface.signal_emitter())
            .await?;
        Ok(())
    }

    fn status(&self) -> String {
        lock(&self.state).status.name().to_string()
    }

    fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::PrivateBus;
    use crate::media::{CALL_LIMIT, PLAYER_INTERFACE, Player as Found, call, list_players};

    /// The app's own reads and calls find the fake, pause and play it, and
    /// see the user's changes; `FakePlayer1` reaches it over D-Bus.
    #[tokio::test]
    async fn the_app_finds_pauses_and_plays_the_fake() {
        let bus = PrivateBus::start().unwrap();
        let player = FakePlayer::serve(bus.address(), "fake").await.unwrap();
        let conn = bus.connect().await.unwrap();
        let found = |status| Found {
            id: player.unique_name(),
            name: player.bus_name().to_string(),
            status,
        };
        assert_eq!(
            list_players(&conn, CALL_LIMIT).await.unwrap(),
            [found(Playback::Stopped)]
        );

        player.set_status(Playback::Playing).await.unwrap();
        assert_eq!(
            list_players(&conn, CALL_LIMIT).await.unwrap(),
            [found(Playback::Playing)]
        );
        call(&conn, &found(Playback::Playing), "Pause", CALL_LIMIT)
            .await
            .unwrap();
        assert_eq!(player.status(), Playback::Paused);
        call(&conn, &found(Playback::Paused), "Play", CALL_LIMIT)
            .await
            .unwrap();
        assert_eq!(player.status(), Playback::Playing);
        assert_eq!(player.calls(), ["Pause", "Play"]);

        let control = zbus::Proxy::new(&conn, player.bus_name(), MPRIS_PATH, CONTROL_INTERFACE)
            .await
            .unwrap();
        control
            .call_method("SetStatus", &("Stopped",))
            .await
            .unwrap();
        assert_eq!(player.status(), Playback::Stopped);
        let status: String = control.call("Status", &()).await.unwrap();
        assert_eq!(status, "Stopped");
        let calls: Vec<String> = control.call("Calls", &()).await.unwrap();
        assert_eq!(calls, ["Pause", "Play"], "the user's are not among them");
        match control.call_method("SetStatus", &("Loud",)).await {
            Err(zbus::Error::MethodError(name, _, _)) => {
                assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs")
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }

        // PlayPause toggles, as MPRIS says; Pause leaves a stopped player
        // stopped.
        let mpris = zbus::Proxy::new(&conn, player.bus_name(), MPRIS_PATH, PLAYER_INTERFACE)
            .await
            .unwrap();
        mpris.call_method("Pause", &()).await.unwrap();
        assert_eq!(player.status(), Playback::Stopped);
        mpris.call_method("PlayPause", &()).await.unwrap();
        assert_eq!(player.status(), Playback::Playing);
        mpris.call_method("PlayPause", &()).await.unwrap();
        assert_eq!(player.status(), Playback::Paused);

        player.close().await.unwrap();
        assert_eq!(list_players(&conn, CALL_LIMIT).await.unwrap(), []);
    }

    /// A player that holds two MPRIS names, as some browsers do, is one
    /// player.
    #[tokio::test]
    async fn a_player_under_two_names_is_found_once() {
        let bus = PrivateBus::start().unwrap();
        let player = FakePlayer::serve(bus.address(), "fake").await.unwrap();
        player
            .conn
            .request_name(format!("{MPRIS_PREFIX}fake.instance2"))
            .await
            .unwrap();
        let conn = bus.connect().await.unwrap();
        let found = list_players(&conn, CALL_LIMIT).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id, player.unique_name());
    }
}
