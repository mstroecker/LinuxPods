//! Low battery notifications, sent as `GNotification`s. What to send is worked
//! out on the tokio side - see `crate::notify` - and arrives here, on the main
//! context, where the application can send it.
//!
//! The Preferences switch turns them off and on, through the application's
//! stateful action [`ACTION`]. It starts as `crate::settings` saved it, and a
//! change to it is saved there.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gtk::{gio, glib};

use gio::prelude::*;

use crate::notify::{Action, Notice};
use crate::settings::Saver;

/// The application action that turns low battery notifications on and off,
/// with a boolean state. The Preferences switch is bound to `app.` and this.
pub(super) const ACTION: &str = "battery-notifications";

/// What the notifier asks for, and what of it is carried out while the
/// switch is on or off.
///
/// Every notice the notifier shows is kept until it withdraws it, whether the
/// switch is on or not: those are the notifications that would be on show
/// with the switch on. Turning it off withdraws them. Turning it on shows what
/// is low as it stands, once each: the notifier keeps tracking crossings
/// meanwhile (`notify::Alerts`), so a crossing it has since taken back - the
/// part charged, or the pair's reading expired - is not warned about again,
/// and a part that went low while the switch was off is not left out. It does
/// show again a notice the user had dismissed: turning notifications on is an
/// explicit request for them.
///
/// Pure: what to carry out is returned, for [`deliver`] to do.
#[derive(Debug)]
struct Shelf {
    enabled: bool,
    /// By notification ID, the last notice shown and not withdrawn since.
    shown: BTreeMap<String, Notice>,
}

impl Shelf {
    /// With the switch on or off, and nothing shown yet.
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shown: BTreeMap::new(),
        }
    }

    /// Takes what the notifier asks for, and returns it if it is to be carried
    /// out now: while the switch is on.
    fn take(&mut self, action: Action) -> Option<Action> {
        match &action {
            Action::Show(notice) => {
                self.shown.insert(notice.id.clone(), notice.clone());
            }
            Action::Withdraw(id) => {
                self.shown.remove(id);
            }
        }
        self.enabled.then_some(action)
    }

    /// Turns the switch on or off, and returns what that takes: every notice
    /// kept shown again, or withdrawn.
    fn set_enabled(&mut self, enabled: bool) -> Vec<Action> {
        if enabled == self.enabled {
            return Vec::new();
        }
        self.enabled = enabled;
        if enabled {
            self.shown.values().cloned().map(Action::Show).collect()
        } else {
            self.shown.keys().cloned().map(Action::Withdraw).collect()
        }
    }
}

/// Carries out what `notices` brings, on the main context, for as long as the
/// channel stays open, and adds [`ACTION`] to `app` to turn that off and on.
///
/// The switch starts as `settings` holds it, before anything is delivered:
/// saved off, nothing is sent. Each change goes to `settings`, which saves it.
///
/// A notification has no default action, so clicking it activates the app,
/// which brings up the window.
pub fn deliver(
    app: &impl IsA<gio::Application>,
    notices: async_channel::Receiver<Action>,
    settings: Saver,
) {
    let app = app.clone().upcast::<gio::Application>();
    let enabled = settings.get().battery_notifications;
    if !enabled {
        tracing::info!("low battery notifications off, as saved");
    }
    let shelf = Rc::new(RefCell::new(Shelf::new(enabled)));

    let action = gio::SimpleAction::new_stateful(ACTION, None, &enabled.to_variant());
    action.connect_change_state(glib::clone!(
        #[strong]
        shelf,
        #[weak]
        app,
        move |action, state| {
            let Some(enabled) = state.and_then(|s| s.get::<bool>()) else {
                return;
            };
            action.set_state(&enabled.to_variant());
            tracing::info!(
                "low battery notifications {}",
                if enabled { "on" } else { "off" }
            );
            settings.change(|s| s.battery_notifications = enabled);
            let todo = shelf.borrow_mut().set_enabled(enabled);
            for action in todo {
                carry_out(&app, action);
            }
        }
    ));
    app.add_action(&action);

    glib::spawn_future_local(async move {
        while let Ok(action) = notices.recv().await {
            let todo = shelf.borrow_mut().take(action);
            if let Some(action) = todo {
                carry_out(&app, action);
            }
        }
    });
}

fn carry_out(app: &gio::Application, action: Action) {
    match &action {
        Action::Show(notice) => {
            let notification = gio::Notification::new(&notice.title);
            notification.set_body(Some(&notice.body));
            notification.set_priority(if notice.critical {
                gio::NotificationPriority::High
            } else {
                gio::NotificationPriority::Normal
            });
            app.send_notification(Some(&notice.id), &notification);
        }
        Action::Withdraw(id) => app.withdraw_notification(id),
    }
    #[cfg(feature = "harness")]
    super::automation::record_notification(&action);
}

#[cfg(test)]
mod tests {
    use bluer::Address;

    use std::time::Instant;

    use super::*;
    use crate::notify::{Alerts, notification_id};
    use crate::podstate::{DataSource, PodState, Snapshot};

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

    fn notice(mac: Address, body: &str) -> Notice {
        Notice {
            mac,
            id: notification_id(mac),
            title: "AirPods battery low".into(),
            body: body.into(),
            critical: false,
        }
    }

    #[test]
    fn on_everything_goes_through() {
        let mut shelf = Shelf::new(true);
        let show = Action::Show(notice(PAIR, "Left 15%"));
        assert_eq!(shelf.take(show.clone()), Some(show));
        let withdraw = Action::Withdraw(notification_id(PAIR));
        assert_eq!(shelf.take(withdraw.clone()), Some(withdraw));
    }

    /// Saved off, the switch starts off: nothing is sent from the start, and
    /// turning it on shows what is low.
    #[test]
    fn off_from_the_start_sends_nothing() {
        let mut shelf = Shelf::new(false);
        assert_eq!(shelf.take(Action::Show(notice(PAIR, "Left 15%"))), None);
        assert_eq!(shelf.set_enabled(false), []);
        assert_eq!(
            shelf.set_enabled(true),
            [Action::Show(notice(PAIR, "Left 15%"))]
        );
    }

    #[test]
    fn off_withdraws_what_is_on_show_and_sends_nothing() {
        let mut shelf = Shelf::new(true);
        shelf.take(Action::Show(notice(PAIR, "Left 15%")));
        shelf.take(Action::Show(notice(OTHER, "Case 9%")));
        shelf.take(Action::Withdraw(notification_id(OTHER)));

        assert_eq!(
            shelf.set_enabled(false),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert_eq!(shelf.set_enabled(false), []);
        assert_eq!(shelf.take(Action::Show(notice(OTHER, "Right 4%"))), None);
        assert_eq!(shelf.take(Action::Withdraw(notification_id(PAIR))), None);
    }

    /// A pair whose reading expired while the switch was off is not brought
    /// back with its old level when it is turned on.
    #[test]
    fn on_does_not_bring_back_a_pair_that_has_gone() {
        let now = Instant::now();
        let mut alerts = Alerts::default();
        let mut shelf = Shelf::new(true);
        let feed = |alerts: &mut Alerts, shelf: &mut Shelf, snapshot: &Snapshot| {
            for action in alerts.update(snapshot, now) {
                shelf.take(action);
            }
        };

        let mut low = PodState::empty(DataSource::Aap);
        low.case_battery = Some(12);
        let with_pair = Snapshot {
            states: [(PAIR, low)].into_iter().collect(),
            ..Snapshot::default()
        };
        feed(&mut alerts, &mut shelf, &with_pair);
        assert_eq!(
            shelf.set_enabled(false),
            [Action::Withdraw(notification_id(PAIR))]
        );

        feed(&mut alerts, &mut shelf, &Snapshot::default());
        assert_eq!(shelf.set_enabled(true), []);
    }

    /// On again, what would be on show is shown, as it stands now: a notice
    /// replaced while off in its latest form, one withdrawn while off not at
    /// all.
    #[test]
    fn on_shows_what_would_be_on_show() {
        let mut shelf = Shelf::new(true);
        shelf.take(Action::Show(notice(PAIR, "Left 15%")));
        shelf.set_enabled(false);
        shelf.take(Action::Show(notice(PAIR, "Left 15%, Right 12%")));
        shelf.take(Action::Show(notice(OTHER, "Case 9%")));
        shelf.take(Action::Withdraw(notification_id(OTHER)));

        assert_eq!(
            shelf.set_enabled(true),
            [Action::Show(notice(PAIR, "Left 15%, Right 12%"))]
        );
        assert_eq!(shelf.set_enabled(true), []);
        let show = Action::Show(notice(OTHER, "Right 4%"));
        assert_eq!(shelf.take(show.clone()), Some(show));
    }
}
