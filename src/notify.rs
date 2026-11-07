//! Low battery notifications (#126): GNOME never warns about a Bluetooth
//! battery, so LinuxPods warns itself.
//!
//! [`Alerts`] works out what to show and what to withdraw from a snapshot and
//! the time - pure, and tested without GTK; [`run`] hands that to the GTK main
//! context, where `ui::notifications` sends it as a `GNotification`. What is
//! warned about, and when: `docs/app/notifications.md`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bluer::Address;
use tokio::sync::watch;

use crate::podstate::{self, Coordinator, DataSource, PodState, Snapshot};

/// At or below this a part is low: UPower's default `PercentageLow`.
pub const LOW: u8 = 20;

/// At or below this a part is critically low: UPower's default
/// `PercentageCritical`.
pub const CRITICAL: u8 = 5;

/// Above this a part warned about is armed again without charging: [`LOW`]
/// and a margin that clears the step its sources can differ by, so a reading
/// flipping across [`LOW`] sends nothing
/// (`docs/app/notifications.md#what-is-warned-about`).
pub const REARM_ABOVE: u8 = 25;

/// How old a BLE reading may be and still warn. An older one may be from a
/// pair still in range, and is not warned about. A reading over AAP is always
/// fresh - see [`fresh`] and `docs/app/notifications.md#what-is-warned-about`.
pub const FRESH: Duration = Duration::from_secs(60);

/// How low a part's battery is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Band {
    Normal,
    Low,
    Critical,
}

impl Band {
    /// The band `level` is in. At a threshold counts as below it, as in UPower.
    pub fn of(level: u8) -> Self {
        if level <= CRITICAL {
            Self::Critical
        } else if level <= LOW {
            Self::Low
        } else {
            Self::Normal
        }
    }
}

/// A part of a pair that has a battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Left,
    Right,
    Case,
}

impl Part {
    /// In the order a notification names them.
    const ALL: [Part; 3] = [Part::Left, Part::Right, Part::Case];

    fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Case => "Case",
        }
    }

    /// The part's level and whether it is charging, as `state` reads them.
    fn reading(self, state: &PodState) -> (Option<u8>, bool) {
        match self {
            Self::Left => (state.left_battery, state.left_charging),
            Self::Right => (state.right_battery, state.right_charging),
            Self::Case => (state.case_battery, state.case_charging),
        }
    }
}

/// A notification to show, or to replace the one with its ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub mac: Address,
    /// See [`notification_id`].
    pub id: String,
    /// The pair's name and how low it is: "AirPods Pro battery low".
    pub title: String,
    /// The low parts and their levels: "Left 9%, Right 12%".
    pub body: String,
    /// Whether a part is at or below [`CRITICAL`].
    pub critical: bool,
}

/// What [`Alerts`] has to be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show the notice, replacing the pair's one if it is on show.
    Show(Notice),
    /// Withdraw the notification with this ID, if it is on show.
    Withdraw(String),
}

/// The ID a pair's notification goes by: one per pair, so that it replaces and
/// withdraws its own and no other pair's.
pub fn notification_id(mac: Address) -> String {
    format!("low-battery-{mac}")
}

/// A part warned about: the band it was warned at, and its last level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Warned {
    band: Band,
    /// Updated with each fresh reading that has one, for the body: the case
    /// reports none while the lid is shut with no pod in it.
    level: u8,
}

/// Which low bands have been warned about, per part of each pair, so that a
/// pair is warned once per crossing rather than once per snapshot. The worst
/// band warned about stands until the part charges or rises above
/// [`REARM_ABOVE`]; a stale reading changes nothing, and a pair with no
/// reading left is withdrawn and forgotten
/// (`docs/app/notifications.md#what-is-warned-about`).
///
/// Pure: the time comes in with each snapshot.
#[derive(Debug, Default)]
pub struct Alerts {
    /// By pair, the parts in [`Part::ALL`]'s order: `None` while armed.
    warned: HashMap<Address, [Option<Warned>; 3]>,
}

impl Alerts {
    /// Takes in a snapshot, as of `now`, and returns what to show and what to
    /// withdraw: for each pair whose low parts changed, its notice, or once it
    /// has none, or no reading at all, the withdrawal of its notification.
    pub fn update(&mut self, snapshot: &Snapshot, now: SystemTime) -> Vec<Action> {
        let mut actions = Vec::new();

        let mut gone: Vec<Address> = self
            .warned
            .keys()
            .filter(|mac| !snapshot.states.contains_key(mac))
            .copied()
            .collect();
        gone.sort_unstable();
        for mac in gone {
            self.warned.remove(&mac);
            actions.push(Action::Withdraw(notification_id(mac)));
        }

        let mut macs: Vec<Address> = snapshot.states.keys().copied().collect();
        macs.sort_unstable();
        for mac in macs {
            let state = &snapshot.states[&mac];
            if !fresh(state, now) {
                continue;
            }
            let before = self.warned.get(&mac).copied().unwrap_or_default();
            let after = judge(before, state);
            if after.iter().all(Option::is_none) {
                if self.warned.remove(&mac).is_some() {
                    actions.push(Action::Withdraw(notification_id(mac)));
                }
                continue;
            }
            self.warned.insert(mac, after);
            // A level that moves within its band is not worth showing again:
            // only a band that changes is.
            if bands(&after) != bands(&before) {
                actions.push(Action::Show(notice(mac, &after, snapshot)));
            }
        }
        actions
    }
}

/// The parts warned about once `state` is taken in, from `before`.
fn judge(before: [Option<Warned>; 3], state: &PodState) -> [Option<Warned>; 3] {
    let mut after = before;
    for (slot, part) in after.iter_mut().zip(Part::ALL) {
        *slot = match part.reading(state) {
            // Charging arms both thresholds again.
            (_, true) => None,
            // No reading: nothing is known, and nothing changes.
            (None, false) => *slot,
            // Clear of the low threshold: armed again.
            (Some(level), false) if level > REARM_ABOVE => None,
            // The worst band reached stands: a part never goes back from
            // critical to low, nor from low to armed, short of that.
            (Some(level), false) => match (*slot, Band::of(level)) {
                (None, Band::Normal) => None,
                (None, band) => Some(Warned { band, level }),
                (Some(warned), band) => Some(Warned {
                    band: warned.band.max(band),
                    level,
                }),
            },
        };
    }
    after
}

/// The band each part was warned at, `None` for one that is armed.
fn bands(parts: &[Option<Warned>; 3]) -> [Option<Band>; 3] {
    parts.map(|p| p.map(|w| w.band))
}

/// Whether `state` tells how the battery is now - see [`FRESH`].
pub fn fresh(state: &PodState, now: SystemTime) -> bool {
    match (state.source, state.last_seen) {
        (DataSource::Aap, _) => true,
        (DataSource::Ble, Some(seen)) => podstate::age(seen, now).is_some_and(|age| age < FRESH),
        // Every BLE reading the coordinator hands out has a time.
        (DataSource::Ble, None) => false,
    }
}

/// A pair's notice, for the parts in `warned`.
fn notice(mac: Address, warned: &[Option<Warned>; 3], snapshot: &Snapshot) -> Notice {
    let critical = warned.iter().flatten().any(|w| w.band == Band::Critical);
    let name = snapshot
        .display_name(mac)
        .unwrap_or_else(|| "AirPods".to_string());
    let title = if critical {
        format!("{name} battery critically low")
    } else {
        format!("{name} battery low")
    };
    let body = Part::ALL
        .iter()
        .zip(warned)
        .filter_map(|(part, w)| w.map(|w| format!("{} {}%", part.label(), w.level)))
        .collect::<Vec<_>>()
        .join(", ");
    Notice {
        mac,
        id: notification_id(mac),
        title,
        body,
        critical,
    }
}

/// Watches the coordinator's snapshots for as long as the app runs, and sends
/// what to show and withdraw to `notices`, for the GTK main context to carry
/// out.
pub async fn run(coordinator: Arc<Coordinator>, notices: async_channel::Sender<Action>) {
    watch_snapshots(coordinator.subscribe(), notices).await;
}

/// [`run`], on `updates`. Returns when either channel closes.
pub async fn watch_snapshots(
    mut updates: watch::Receiver<Snapshot>,
    notices: async_channel::Sender<Action>,
) {
    let mut alerts = Alerts::default();
    while updates.changed().await.is_ok() {
        // Cloned out: the borrow is a read lock every broadcast waits for, and
        // the sends below are awaited.
        let snapshot = updates.borrow_and_update().clone();
        for action in alerts.update(&snapshot, SystemTime::now()) {
            match &action {
                Action::Show(notice) => {
                    tracing::info!("battery low on {}: {}", notice.mac, notice.body);
                }
                Action::Withdraw(id) => {
                    tracing::info!("battery no longer low: withdrawing {id}");
                }
            }
            if notices.send(action).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluez::DeviceNames;
    use crate::podstate::Identity;

    const PAIR: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

    /// A reading over `source` of left, right and case, none charging.
    fn reading(source: DataSource, [left, right, case]: [Option<u8>; 3]) -> PodState {
        let mut state = PodState::empty(source);
        state.left_battery = left;
        state.right_battery = right;
        state.case_battery = case;
        state
    }

    fn aap(levels: [Option<u8>; 3]) -> PodState {
        reading(DataSource::Aap, levels)
    }

    fn snapshot(states: impl IntoIterator<Item = (Address, PodState)>) -> Snapshot {
        Snapshot {
            states: states.into_iter().collect(),
            ..Snapshot::default()
        }
    }

    fn shown(actions: &[Action]) -> Vec<(&str, &str)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Show(n) => Some((n.title.as_str(), n.body.as_str())),
                Action::Withdraw(_) => None,
            })
            .collect()
    }

    #[test]
    fn thresholds_are_upowers_and_inclusive() {
        assert_eq!(Band::of(100), Band::Normal);
        assert_eq!(Band::of(21), Band::Normal);
        assert_eq!(Band::of(20), Band::Low);
        assert_eq!(Band::of(6), Band::Low);
        assert_eq!(Band::of(5), Band::Critical);
        assert_eq!(Band::of(0), Band::Critical);
    }

    /// A crossing into the low band warns once: the same low reading again,
    /// and lower ones within the band, warn no more.
    #[test]
    fn a_crossing_warns_once() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        assert!(
            alerts
                .update(&snapshot([(PAIR, aap([Some(21), Some(80), None]))]), now)
                .is_empty()
        );
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(20), Some(80), None]))]), now);
        assert_eq!(shown(&actions), [("AirPods battery low", "Left 20%")]);
        for level in [20, 19, 12, 6] {
            let actions =
                alerts.update(&snapshot([(PAIR, aap([Some(level), Some(80), None]))]), now);
            assert!(actions.is_empty(), "{level}: {actions:?}");
        }
    }

    #[test]
    fn critical_warns_again_and_names_every_low_part() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        alerts.update(&snapshot([(PAIR, aap([Some(9), Some(80), None]))]), now);
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(9), Some(12), None]))]), now);
        assert_eq!(
            shown(&actions),
            [("AirPods battery low", "Left 9%, Right 12%")]
        );
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(4), Some(11), None]))]), now);
        assert_eq!(
            shown(&actions),
            [("AirPods battery critically low", "Left 4%, Right 11%")]
        );
        let Action::Show(notice) = &actions[0] else {
            unreachable!()
        };
        assert!(notice.critical);
        assert_eq!(notice.id, "low-battery-00:00:5E:00:53:01");
    }

    /// Straight from above both thresholds to below the critical one is one
    /// crossing, and one warning.
    #[test]
    fn a_drop_past_both_thresholds_warns_once_as_critical() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(3), None, None]))]), now);
        assert_eq!(
            shown(&actions),
            [("AirPods battery critically low", "Left 3%")]
        );
    }

    #[test]
    fn charging_withdraws_and_arms_again() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        alerts.update(&snapshot([(PAIR, aap([Some(15), None, None]))]), now);
        let mut charging = aap([Some(15), None, None]);
        charging.left_charging = true;
        let actions = alerts.update(&snapshot([(PAIR, charging)]), now);
        assert_eq!(
            actions,
            [Action::Withdraw("low-battery-00:00:5E:00:53:01".into())]
        );
        // Taken off the charger still low, it warns again.
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(16), None, None]))]), now);
        assert_eq!(shown(&actions), [("AirPods battery low", "Left 16%")]);
    }

    #[test]
    fn rising_clear_of_the_low_threshold_arms_it_again() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        let left = |level| snapshot([(PAIR, aap([Some(level), None, None]))]);
        alerts.update(&left(4), now);
        // Back above either threshold, but not clear of the low one: the
        // critical warning stands.
        for level in [7, 21, REARM_ABOVE] {
            let actions = alerts.update(&left(level), now);
            assert!(actions.is_empty(), "{level}: {actions:?}");
        }
        // Clear of it: withdrawn, and the next crossing warns.
        let actions = alerts.update(&left(REARM_ABOVE + 1), now);
        assert!(matches!(actions[..], [Action::Withdraw(_)]));
        let actions = alerts.update(&left(20), now);
        assert_eq!(shown(&actions), [("AirPods battery low", "Left 20%")]);
    }

    /// Readings that flip across a threshold - two pods' views of one level,
    /// or AAP and BLE a step apart - warn once, and never withdraw.
    #[test]
    fn a_reading_flipping_across_a_threshold_warns_once() {
        let now = SystemTime::now();
        let left = |level| snapshot([(PAIR, aap([Some(level), None, None]))]);
        for (levels, warning) in [
            ([20, 21], ("AirPods battery low", "Left 20%")),
            ([5, 6], ("AirPods battery critically low", "Left 5%")),
        ] {
            let mut alerts = Alerts::default();
            let mut sent = Vec::new();
            for level in levels.into_iter().cycle().take(20) {
                sent.extend(alerts.update(&left(level), now));
            }
            assert_eq!(sent.len(), 1, "{levels:?}: {sent:?}");
            assert_eq!(shown(&sent), [warning], "{levels:?}");
        }

        // Low first, then flipping across the critical threshold: one more
        // warning, as critical, and nothing after it.
        let mut alerts = Alerts::default();
        let mut sent = alerts.update(&left(12), now);
        for level in [6, 5].into_iter().cycle().take(20) {
            sent.extend(alerts.update(&left(level), now));
        }
        assert_eq!(
            shown(&sent),
            [
                ("AirPods battery low", "Left 12%"),
                ("AirPods battery critically low", "Left 5%")
            ]
        );
        assert_eq!(sent.len(), 2, "{sent:?}");
    }

    /// One part charging leaves the other on show, with the notice replaced.
    #[test]
    fn a_part_that_charges_leaves_the_notice() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        alerts.update(&snapshot([(PAIR, aap([Some(9), Some(12), None]))]), now);
        let mut left_charging = aap([Some(9), Some(12), None]);
        left_charging.left_charging = true;
        let actions = alerts.update(&snapshot([(PAIR, left_charging)]), now);
        assert_eq!(shown(&actions), [("AirPods battery low", "Right 12%")]);
    }

    /// The case reports a level only while a pod is in it or the lid is open.
    /// No level is no news: it neither warns nor arms.
    #[test]
    fn the_case_is_warned_about_only_from_a_reading() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(80), Some(80), None]))]), now);
        assert!(actions.is_empty());
        let actions = alerts.update(
            &snapshot([(PAIR, aap([Some(80), Some(80), Some(10)]))]),
            now,
        );
        assert_eq!(shown(&actions), [("AirPods battery low", "Case 10%")]);
        // The lid shut: no level, so no withdrawal and no second warning.
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(80), Some(80), None]))]), now);
        assert!(actions.is_empty(), "{actions:?}");
        let actions = alerts.update(&snapshot([(PAIR, aap([Some(80), Some(80), Some(9)]))]), now);
        assert!(actions.is_empty(), "{actions:?}");
    }

    /// A cached advertisement of a pair that has gone - in a drawer, say -
    /// says nothing about its battery now.
    #[test]
    fn only_a_fresh_ble_reading_warns() {
        let now = SystemTime::now();
        let ble = |age: Duration| {
            let mut state = reading(DataSource::Ble, [Some(10), Some(10), None]);
            state.last_seen = Some(now - age);
            state
        };

        let mut alerts = Alerts::default();
        let stale = alerts.update(&snapshot([(PAIR, ble(FRESH))]), now);
        assert!(stale.is_empty(), "{stale:?}");
        let stale = alerts.update(&snapshot([(PAIR, ble(Duration::from_secs(25 * 60)))]), now);
        assert!(stale.is_empty(), "{stale:?}");

        // Heard after now - the clock set back since - is no telling how old.
        let mut ahead = reading(DataSource::Ble, [Some(10), Some(10), None]);
        ahead.last_seen = Some(now + Duration::from_secs(2));
        let stale = alerts.update(&snapshot([(PAIR, ahead)]), now);
        assert!(stale.is_empty(), "{stale:?}");

        let actions = alerts.update(&snapshot([(PAIR, ble(Duration::from_secs(2)))]), now);
        assert_eq!(
            shown(&actions),
            [("AirPods battery low", "Left 10%, Right 10%")]
        );

        // Gone, its reading aging in the cache: nothing is withdrawn on the
        // strength of it, nor armed.
        let mut charging = ble(Duration::from_secs(10 * 60));
        charging.left_charging = true;
        assert!(alerts.update(&snapshot([(PAIR, charging)]), now).is_empty());
        let mut alerts = Alerts::default();
        assert!(
            alerts
                .update(
                    &snapshot([(PAIR, reading(DataSource::Ble, [Some(10); 3]))]),
                    now
                )
                .is_empty()
        );
    }

    /// A pair whose reading has left the snapshot - its advertisement expired
    /// from the cache - is withdrawn, once, and starts over if it comes back.
    #[test]
    fn a_pair_without_a_reading_is_withdrawn() {
        let now = SystemTime::now();
        let mut alerts = Alerts::default();
        alerts.update(
            &snapshot([
                (PAIR, aap([Some(80), Some(80), Some(12)])),
                (OTHER, aap([Some(15), None, None])),
            ]),
            now,
        );

        // Gone quiet: stale, the notice stays.
        let mut quiet = reading(DataSource::Ble, [Some(80), Some(80), Some(12)]);
        quiet.last_seen = Some(now - Duration::from_secs(10 * 60));
        let other = (OTHER, aap([Some(15), None, None]));
        assert!(
            alerts
                .update(&snapshot([(PAIR, quiet), other.clone()]), now)
                .is_empty()
        );

        // Expired: withdrawn, and the other pair's notice left alone.
        let only_other = snapshot([other.clone()]);
        assert_eq!(
            alerts.update(&only_other, now),
            [Action::Withdraw(notification_id(PAIR))]
        );
        assert!(alerts.update(&only_other, now).is_empty());

        // Heard again, still low: warned about anew.
        let actions = alerts.update(
            &snapshot([(PAIR, aap([Some(80), Some(80), Some(11)])), other]),
            now,
        );
        assert_eq!(shown(&actions), [("AirPods battery low", "Case 11%")]);
    }

    #[test]
    fn each_pair_has_its_own_notice() {
        let mut alerts = Alerts::default();
        let now = SystemTime::now();
        let mut snap = snapshot([
            (PAIR, aap([Some(15), Some(50), None])),
            (OTHER, aap([Some(50), Some(4), None])),
        ]);
        let names = DeviceNames {
            alias: "Marcel’s AirPods".into(),
            name: None,
        };
        snap.device_names.insert(PAIR, names);
        let mut other = snap.states[&OTHER].clone();
        other.identity = Some(Identity {
            product_id: 0x2024,
            color: 0,
        });
        snap.states.insert(OTHER, other);

        let actions = alerts.update(&snap, now);
        assert_eq!(
            actions
                .iter()
                .map(|a| match a {
                    Action::Show(n) => (n.id.as_str(), n.title.as_str(), n.body.as_str()),
                    Action::Withdraw(_) => unreachable!(),
                })
                .collect::<Vec<_>>(),
            [
                (
                    "low-battery-00:00:5E:00:53:01",
                    "Marcel’s AirPods battery low",
                    "Left 15%"
                ),
                (
                    "low-battery-00:00:5E:00:53:02",
                    "AirPods Pro (2nd gen, USB-C) battery critically low",
                    "Right 4%"
                ),
            ]
        );

        // One pair charging leaves the other's notice alone.
        let mut charging = snap.states[&PAIR].clone();
        charging.left_charging = true;
        snap.states.insert(PAIR, charging);
        assert_eq!(
            alerts.update(&snap, now),
            [Action::Withdraw("low-battery-00:00:5E:00:53:01".into())]
        );

        // Renamed, with no alias set: BlueZ's alias is the old name until the
        // pair connects again, and the notice names the pods' new one.
        let mut alerts = Alerts::default();
        let mut renamed = snapshot([(PAIR, aap([Some(15), Some(50), None]))]);
        renamed.states.get_mut(&PAIR).unwrap().name = Some("Test Pods".into());
        let names = DeviceNames {
            alias: "Marcel’s AirPods".into(),
            name: Some("Marcel’s AirPods".into()),
        };
        renamed.device_names.insert(PAIR, names);
        assert_eq!(
            shown(&alerts.update(&renamed, now)),
            [("Test Pods battery low", "Left 15%")]
        );
    }

    /// The pipeline from a fake pair's AAP battery report to the notice:
    /// below the threshold it is shown, charging withdraws it.
    #[cfg(feature = "harness")]
    mod pipeline {
        use super::*;
        use crate::attachment::Attachment;
        use crate::harness::pods::{PRO_3, Part as PodPart};
        use crate::harness::{PrivateBus, World};

        const TIMEOUT: Duration = Duration::from_secs(15);

        async fn next(notices: &async_channel::Receiver<Action>) -> Action {
            tokio::time::timeout(TIMEOUT, notices.recv())
                .await
                .expect("timed out waiting for a notice")
                .expect("the notifier runs")
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_low_battery_over_aap_is_shown_and_withdrawn() {
            let bus = PrivateBus::start().unwrap();
            let sockets = tempfile::Builder::new()
                .prefix("linuxpods-aap")
                .tempdir()
                .unwrap();
            let world = World::new(bus.address(), sockets.path()).await.unwrap();
            world.add_pods(PAIR, PRO_3, "Fake AirPods").await.unwrap();
            world.publish().await.unwrap();

            // No keystore: nothing is saved, least of all to the app's data
            // directory.
            let coordinator = Coordinator::with_aap_socket_dir(None, sockets.path().to_path_buf());
            let mut updates = coordinator.subscribe();
            let attachment = Attachment::new(
                bus.connect_for_providers().await.unwrap(),
                coordinator.clone(),
            )
            .await
            .unwrap();
            let attaching = tokio::spawn(attachment.run());

            let (tx, notices) = async_channel::unbounded();
            let watching = tokio::spawn(watch_snapshots(coordinator.subscribe(), tx));

            world.connect(PAIR).await.unwrap();
            tokio::time::timeout(
                TIMEOUT,
                updates.wait_for(|s| {
                    s.states
                        .get(&PAIR)
                        .is_some_and(|p| p.source == DataSource::Aap)
                }),
            )
            .await
            .expect("timed out waiting for the AAP reading")
            .unwrap();

            world
                .with_pods(PAIR, |pods| pods.set_battery(PodPart::Left, Some(15)))
                .await
                .unwrap();
            let Action::Show(notice) = next(&notices).await else {
                panic!("expected a notice");
            };
            assert_eq!(notice.id, notification_id(PAIR));
            assert!(notice.title.ends_with(" battery low"), "{notice:?}");
            assert_eq!(notice.body, "Left 15%");

            world
                .with_pods(PAIR, |pods| pods.set_charging(PodPart::Left, true))
                .await
                .unwrap();
            assert_eq!(
                next(&notices).await,
                Action::Withdraw(notification_id(PAIR))
            );

            watching.abort();
            attaching.abort();
        }
    }
}
