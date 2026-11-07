//! The scanning hint in Preferences: shown while LinuxPods listens by
//! discovery, with a button that pauses it and a link to the user document.
//! Why it is shown, and how it is drawn:
//! [app/ui.md](../../docs/app/ui.md#the-scanning-hint).

use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};
use tokio::time::Instant;

use crate::ble::scanner::{DISCOVERY_PAUSE, ScanControl, ScanState};

/// The user document the hint links, as it stands on `main`.
const GUIDE: &str = "https://github.com/mstroecker/LinuxPods/blob/main/docs/user/other-devices.md";

/// An icon for a link that leaves the app, bundled with libadwaita: found in
/// libadwaita 1.10, not checked against the oldest version the build takes.
const EXTERNAL_LINK_ICON: &str = "adw-external-link-symbolic";

/// What the hint shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hint {
    /// Nothing: no powered adapter listens by discovery.
    Hidden,
    /// The hint, with the app's discovery not paused.
    Scanning,
    /// The hint, with the app's discovery paused for `left` more.
    Paused { left: Duration },
}

/// What the hint shows for `state` at `now`. A pause whose end has passed
/// shows as none, before the scanner has said that it is over.
pub(super) fn hint(state: ScanState, now: Instant) -> Hint {
    if !state.fallback {
        return Hint::Hidden;
    }
    match state.paused_until {
        Some(until) if until > now => Hint::Paused { left: until - now },
        _ => Hint::Scanning,
    }
}

/// The hint's group, which the Preferences page holds, and the row that says
/// whether the discovery is paused.
pub(super) struct ScanHint {
    group: adw::PreferencesGroup,
    pause_row: adw::ActionRow,
}

impl ScanHint {
    /// The hint, hidden until [`ScanHint::show`] is given a state that shows
    /// it. Its button asks `control` for the pause; what the hint shows of
    /// the pause comes back with the state.
    pub(super) fn new(control: ScanControl) -> Self {
        let group = adw::PreferencesGroup::builder()
            .name("scan-hint")
            .title("Other Bluetooth Devices")
            .description(
                "Mice and similar devices may not reconnect while LinuxPods scans for \
                 AirPods. Pausing the scan lets them connect: AirPods nearby go unheard \
                 meanwhile, and another app scanning, such as GNOME Settings’ Bluetooth \
                 panel, still blocks them.",
            )
            .visible(false)
            .build();

        let pause = gtk::Button::builder()
            .name("scan-pause")
            .label(format!(
                "Pause scanning for {} s",
                DISCOVERY_PAUSE.as_secs()
            ))
            .valign(gtk::Align::Center)
            .build();
        pause.connect_clicked(move |_| control.pause_discovery());
        let pause_row = adw::ActionRow::builder().name("scan-pause-row").build();
        pause_row.add_suffix(&pause);
        group.add(&pause_row);

        let guide = adw::ActionRow::builder()
            .name("scan-guide")
            .title("Let them reconnect")
            .subtitle("How to have LinuxPods listen without scanning, in the browser")
            .activatable(true)
            .build();
        guide.add_suffix(&gtk::Image::from_icon_name(EXTERNAL_LINK_ICON));
        guide.connect_activated(|row| {
            let window = row.root().and_downcast::<gtk::Window>();
            gtk::show_uri(window.as_ref(), GUIDE, gdk::CURRENT_TIME);
        });
        group.add(&guide);

        Self { group, pause_row }
    }

    pub(super) fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Draws `state`: the hint shown or hidden, and whether the discovery is
    /// paused, until when in local time.
    pub(super) fn show(&self, state: ScanState) {
        let title = match hint(state, Instant::now()) {
            Hint::Hidden => {
                self.group.set_visible(false);
                return;
            }
            Hint::Scanning => "Scanning".to_string(),
            Hint::Paused { left } => glib::DateTime::now_local()
                .and_then(|now| now.add_seconds(left.as_secs_f64()))
                .and_then(|until| until.format("%X"))
                .map_or_else(
                    |_| "Paused".to_string(),
                    |until| format!("Paused until {until}"),
                ),
        };
        self.pause_row.set_title(&title);
        self.group.set_visible(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAUSE: Duration = Duration::from_secs(20);

    /// Every powered adapter on a monitor shows nothing, paused or not.
    #[test]
    fn no_fallback_hides_the_hint() {
        let now = Instant::now();
        for paused_until in [None, Some(now + PAUSE)] {
            let state = ScanState {
                fallback: false,
                paused_until,
            };
            assert_eq!(hint(state, now), Hint::Hidden);
        }
    }

    #[test]
    fn a_fallback_shows_the_hint() {
        let state = ScanState {
            fallback: true,
            paused_until: None,
        };
        assert_eq!(hint(state, Instant::now()), Hint::Scanning);
    }

    /// A pause shows what is left of it; one whose end has come shows as
    /// none.
    #[test]
    fn a_pause_shows_until_its_end() {
        let start = Instant::now();
        let state = ScanState {
            fallback: true,
            paused_until: Some(start + PAUSE),
        };
        assert_eq!(hint(state, start), Hint::Paused { left: PAUSE });
        assert_eq!(
            hint(state, start + Duration::from_secs(15)),
            Hint::Paused {
                left: Duration::from_secs(5)
            }
        );
        assert_eq!(hint(state, start + PAUSE), Hint::Scanning);
        assert_eq!(hint(state, start + 2 * PAUSE), Hint::Scanning);
    }
}
