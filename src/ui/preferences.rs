//! The Preferences dialog, and the Development group the update loop fills.

use std::cell::RefCell;
use std::collections::HashMap;

use adw::prelude::*;
use bluer::Address;

use super::{RECENT, last_seen_text};
use crate::podstate::{PodState, Snapshot};

/// The Preferences dialog, and its Development group for the update loop to
/// fill.
pub(super) fn create_preferences_dialog() -> (adw::PreferencesDialog, DeviceRows) {
    let page = adw::PreferencesPage::new();

    // Neither setting exists yet: the app always connects, and nothing watches
    // for a low battery.
    let settings_group = adw::PreferencesGroup::builder().title("General").build();
    for (id, title) in [
        ("auto-connect", "Auto-connect"),
        ("battery-notifications", "Battery notifications"),
    ] {
        settings_group.add(&unavailable_row(id, title));
    }
    page.add(&settings_group);

    let dev_group = adw::PreferencesGroup::builder()
        .title("Development")
        .description("Encryption keys for decrypting BLE advertisements")
        .build();
    page.add(&dev_group);

    let dialog = adw::PreferencesDialog::builder()
        .name("preferences")
        .build();
    dialog.add(&page);

    let devices = DeviceRows {
        group: dev_group,
        rows: RefCell::default(),
        shown: RefCell::default(),
    };
    (dialog, devices)
}

/// A setting that is planned but not built, in Preferences and on the Control
/// page. Shown so the plan is visible, but switched off and greyed out: it
/// neither claims a state nor takes a click, and nothing listens to the switch.
/// Once a feature lands, its state comes with the snapshot and a change goes
/// out as a command, as noise control's does.
///
/// `id` names the switch, so that a test can find it and see it refuse.
pub(super) fn unavailable_row(id: &str, title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle("Not available yet")
        .sensitive(false)
        .build();
    row.add_suffix(
        &gtk::Switch::builder()
            .name(id)
            .active(false)
            .valign(gtk::Align::Center)
            .build(),
    );
    row
}

/// The Development group: a row per device, by real MAC.
pub(super) struct DeviceRows {
    group: adw::PreferencesGroup,
    rows: RefCell<HashMap<Address, DeviceRow>>,
    /// The devices whose rows the group holds, top to bottom.
    shown: RefCell<Vec<Address>>,
}

impl DeviceRows {
    /// Puts a row in the group for every device the snapshot lists, in its
    /// order, redraws them all, and drops the rows of devices that are gone.
    pub(super) fn update(&self, snapshot: &Snapshot) {
        let listed = row_order(snapshot);
        let mut rows = self.rows.borrow_mut();
        for &(mac, known) in &listed {
            let row = rows.entry(mac).or_insert_with(|| DeviceRow::new(mac));
            row.show(
                mac,
                snapshot.states.get(&mac),
                known,
                snapshot.is_connected(mac),
            );
        }

        // The group can only append, so a device listed above others takes
        // their rows off and puts them back after its own. Every device in
        // either list has a row: the listed ones got theirs above, and a shown
        // one's is dropped only below.
        let wanted: Vec<Address> = listed.iter().map(|&(mac, _)| mac).collect();
        let mut shown = self.shown.borrow_mut();
        let moves = moves(&shown, &wanted);
        for mac in &moves.remove {
            self.group.remove(&rows[mac].row);
        }
        for mac in &moves.append {
            self.group.add(&rows[mac].row);
        }
        *shown = wanted;

        // Drop a row once the device is no longer listed: no key, and no link.
        rows.retain(|mac, _| shown.contains(mac));
    }
}

/// The rows to take off the group, and the rows to add back at its end.
#[derive(Debug, Default, PartialEq)]
struct Moves {
    /// The rows to take off: those of devices gone, and those that move.
    remove: Vec<Address>,
    /// The rows to add at the end, in order: those that move, and new ones.
    append: Vec<Address>,
}

/// What brings the group from showing `shown` to showing `wanted`, top to
/// bottom. Every row below the first one out of place comes off and goes back
/// on at the end; the rows above it stay, and an unchanged list moves nothing.
fn moves(shown: &[Address], wanted: &[Address]) -> Moves {
    // Taking a row off leaves the others in order, so the rows of devices gone
    // do not count.
    let staying = shown.iter().filter(|mac| wanted.contains(mac));
    let in_place = staying.zip(wanted).take_while(|(s, w)| s == w).count();
    Moves {
        remove: shown
            .iter()
            .filter(|mac| !wanted[..in_place].contains(mac))
            .copied()
            .collect(),
        append: wanted[in_place..].to_vec(),
    }
}

/// The devices to list - the snapshot's, in its order - each with whether a key
/// is stored for it. A pair on AAP whose key has not arrived is among them.
fn row_order(snapshot: &Snapshot) -> Vec<(Address, bool)> {
    snapshot
        .devices()
        .into_iter()
        .map(|mac| (mac, snapshot.known_keys.contains(&mac)))
        .collect()
}

/// One device in the Development group.
struct DeviceRow {
    row: adw::ActionRow,
    /// Connection state for this device: Connected / Advertising / Idle.
    status_label: gtk::Label,
}

impl DeviceRow {
    fn new(mac: Address) -> Self {
        // Title and subtitle carry device data, which must never be read as
        // Pango markup - libadwaita's default. The IDs end in the MAC: there is
        // a row per device.
        let row = adw::ActionRow::builder()
            .name(format!("device-{mac}"))
            .title(mac.to_string())
            .use_markup(false)
            .build();

        let status_label = gtk::Label::builder()
            .name(format!("device-status-{mac}"))
            .label("Idle")
            .build();
        status_label.add_css_class("dim-label");
        status_label.set_valign(gtk::Align::Center);
        status_label.set_margin_end(8);
        row.add_suffix(&status_label);

        DeviceRow { row, status_label }
    }

    /// Draws the device's address, model and connection state. `known` is
    /// whether a key is stored for it.
    fn show(&self, mac: Address, state: Option<&PodState>, known: bool, connected: bool) {
        self.row.set_title(&row_title(mac, state));
        self.row.set_subtitle(&row_subtitle(state));

        let (text, css) = row_status(known, connected, state);
        self.status_label.set_text(&text);
        for class in ["success", "warning", "dim-label"] {
            self.status_label.remove_css_class(class);
        }
        self.status_label.add_css_class(css);
    }
}

/// The real address, and the rotating BLE one alongside it when they differ.
fn row_title(mac: Address, state: Option<&PodState>) -> String {
    match state.and_then(|s| s.current_ble_mac) {
        Some(ble_mac) if ble_mac != mac => format!("{mac} • BLE: {ble_mac}"),
        _ => mac.to_string(),
    }
}

/// The model, or nothing until an advertisement has identified the device.
fn row_subtitle(state: Option<&PodState>) -> String {
    state
        .and_then(|s| s.identity)
        .map(|identity| identity.model_name())
        .unwrap_or_default()
}

/// A row's status text, and the style class to draw it in.
fn row_status(known: bool, connected: bool, state: Option<&PodState>) -> (String, &'static str) {
    match (known, connected, state) {
        (_, true, _) => ("Connected".to_string(), "success"),
        // Listed without a stored key. Only a device on AAP is - its key is
        // requested on connecting - and `Connected` above takes it first, so
        // this is a fallback.
        (false, _, _) => ("No key".to_string(), "warning"),
        // A cached reading is not an advertisement in progress.
        (true, _, Some(s)) => match s.last_seen.map(|t| t.elapsed()) {
            Some(age) if age >= RECENT => (last_seen_text(age), "dim-label"),
            _ => ("Advertising".to_string(), "dim-label"),
        },
        (true, _, None) => ("Idle".to_string(), "dim-label"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::podstate::{DataSource, Identity};

    const A: Address = Address::new([0xAA; 6]);
    const B: Address = Address::new([0xBB; 6]);
    const C: Address = Address::new([0xCC; 6]);
    const D: Address = Address::new([0xDD; 6]);

    /// A reading heard `age` ago.
    fn heard(age: Duration) -> PodState {
        PodState {
            last_seen: Some(Instant::now() - age),
            ..PodState::empty(DataSource::Ble)
        }
    }

    /// A row per device the snapshot lists, in MAC order: one with a key but no
    /// reading, one on AAP without a key, and one with both, listed once.
    #[test]
    fn rows_follow_the_device_list() {
        let snapshot = Snapshot {
            known_keys: vec![B, D],
            connected_macs: vec![C, B],
            states: [B, C]
                .into_iter()
                .map(|m| (m, PodState::empty(DataSource::Aap)))
                .collect(),
            ..Default::default()
        };
        assert_eq!(row_order(&snapshot), [(B, true), (C, false), (D, true)]);
    }

    /// Updates arrive on every advertisement, and almost all of them list the
    /// same devices.
    #[test]
    fn an_unchanged_list_moves_no_row() {
        assert_eq!(moves(&[A, B, C], &[A, B, C]), Moves::default());
        assert_eq!(moves(&[], &[]), Moves::default());
    }

    /// A device listed later lands in MAC order: the rows below its place move
    /// down, and the rows above it stay put.
    #[test]
    fn a_new_device_moves_the_rows_below_it() {
        assert_eq!(
            moves(&[A, C, D], &[A, B, C, D]),
            Moves {
                remove: vec![C, D],
                append: vec![B, C, D],
            }
        );
        assert_eq!(
            moves(&[A, B], &[A, B, C]),
            Moves {
                remove: vec![],
                append: vec![C],
            }
        );
    }

    #[test]
    fn a_device_gone_takes_off_only_its_own_row() {
        assert_eq!(
            moves(&[A, B, C], &[A, C]),
            Moves {
                remove: vec![B],
                append: vec![],
            }
        );
    }

    /// From any list of devices to any other, taking the rows off and appending
    /// them leaves the group in the wanted order.
    #[test]
    fn moves_end_in_the_wanted_order() {
        let lists: Vec<Vec<Address>> = (0..16)
            .map(|bits| {
                [A, B, C, D]
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| bits & (1 << i) != 0)
                    .map(|(_, mac)| mac)
                    .collect()
            })
            .collect();
        for shown in &lists {
            for wanted in &lists {
                let moves = moves(shown, wanted);
                let mut group = shown.clone();
                group.retain(|mac| !moves.remove.contains(mac));
                group.extend(&moves.append);
                assert_eq!(&group, wanted, "from {shown:?}");
            }
        }
    }

    #[test]
    fn title_adds_a_differing_ble_address() {
        let rotating = PodState {
            current_ble_mac: Some(D),
            ..PodState::empty(DataSource::Ble)
        };
        assert_eq!(
            row_title(A, Some(&rotating)),
            "AA:AA:AA:AA:AA:AA • BLE: DD:DD:DD:DD:DD:DD"
        );

        let same = PodState {
            current_ble_mac: Some(A),
            ..PodState::empty(DataSource::Ble)
        };
        assert_eq!(row_title(A, Some(&same)), "AA:AA:AA:AA:AA:AA");
        assert_eq!(
            row_title(A, Some(&PodState::empty(DataSource::Aap))),
            "AA:AA:AA:AA:AA:AA"
        );
        assert_eq!(row_title(A, None), "AA:AA:AA:AA:AA:AA");
    }

    /// The model's name; the product ID for a model we do not know; and
    /// nothing while no advertisement has identified the device.
    #[test]
    fn subtitle_names_the_model_as_far_as_it_is_known() {
        let identified = |product_id| PodState {
            identity: Some(Identity {
                product_id,
                color: 0x00,
            }),
            ..PodState::empty(DataSource::Ble)
        };
        assert_eq!(row_subtitle(Some(&identified(0x2027))), "AirPods Pro 3");
        assert_eq!(row_subtitle(Some(&identified(0xFFFF))), "Unknown (0xFFFF)");
        assert_eq!(row_subtitle(Some(&PodState::empty(DataSource::Aap))), "");
        assert_eq!(row_subtitle(None), "");
    }

    /// Connected wins over everything, even a missing key: the key is requested
    /// as the link comes up.
    #[test]
    fn status_shows_a_connection_first() {
        let reading = heard(Duration::from_secs(5 * 60));
        for (known, state) in [(true, Some(&reading)), (false, None), (true, None)] {
            assert_eq!(
                row_status(known, true, state),
                ("Connected".to_string(), "success")
            );
        }
    }

    #[test]
    fn status_flags_a_missing_key() {
        let reading = heard(Duration::from_secs(5));
        assert_eq!(
            row_status(false, false, Some(&reading)),
            ("No key".to_string(), "warning")
        );
    }

    /// A fresh reading is an advertisement in progress; an older cached one says
    /// how old it is instead.
    #[test]
    fn status_tells_a_live_advertisement_from_a_cached_one() {
        let fresh = heard(Duration::from_secs(5));
        assert_eq!(
            row_status(true, false, Some(&fresh)),
            ("Advertising".to_string(), "dim-label")
        );

        let cached = heard(Duration::from_secs(2 * 60));
        assert_eq!(
            row_status(true, false, Some(&cached)),
            ("Last seen 2 min ago".to_string(), "dim-label")
        );

        assert_eq!(
            row_status(true, false, None),
            ("Idle".to_string(), "dim-label")
        );
    }
}
