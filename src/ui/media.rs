//! The switch for pausing the media when a pod leaves an ear, and playing it
//! again once it is back. That is done on the tokio side - see
//! `crate::media` - which the switch reaches over a watch channel.
//!
//! The Preferences switch turns it off and on, through the application's
//! stateful action [`ACTION`]. It starts as `crate::settings` saved it, and a
//! change to it is saved there.

use gtk::gio;

use gio::prelude::*;
use tokio::sync::watch;

use crate::media::Switch;
use crate::settings::Saver;

/// The application action that turns pausing on the pods' places on and off,
/// with a boolean state. The Preferences switch is bound to `app.` and this.
pub(super) const ACTION: &str = "pause-on-ear";

/// Adds the action `app.pause-on-ear` to `app`, starting as `settings` holds
/// it. Each change goes to `settings`, which saves it, and to `switch`,
/// which `crate::media::Media` reads: turned off, it forgets what it paused,
/// even if turned on again before it looked, and pauses and plays nothing
/// while off.
pub fn add_action(
    app: &impl IsA<gio::Application>,
    settings: Saver,
    switch: watch::Sender<Switch>,
) {
    let app = app.clone().upcast::<gio::Application>();
    let on = settings.get().pause_on_ear;
    if !on {
        tracing::info!("pausing the media when a pod leaves an ear off, as saved");
    }
    switch.send_modify(|switch| switch.set(on));

    let action = gio::SimpleAction::new_stateful(ACTION, None, &on.to_variant());
    action.connect_change_state(move |action, state| {
        let Some(on) = state.and_then(|s| s.get::<bool>()) else {
            return;
        };
        action.set_state(&on.to_variant());
        tracing::info!(
            "pausing the media when a pod leaves an ear {}",
            if on { "on" } else { "off" }
        );
        settings.change(|s| s.pause_on_ear = on);
        // Never waits: the watch keeps the latest alone, and the switch
        // counts a turning off that a quick turn on would hide.
        switch.send_modify(|switch| switch.set(on));
    });
    app.add_action(&action);
}
