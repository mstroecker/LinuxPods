//! The rename dialog: one entry for the pods' new name, opened from the Name
//! row in the device page's Info group (#160).
//!
//! The name is the pods' own - the one they report to a host - and never
//! BlueZ's alias, which this computer keeps for itself: the body says when the
//! rest of this computer - GNOME Settings, say - shows the new name, and that
//! Apple devices keep theirs. A name the pods would not take is refused here,
//! as `aap::name` refuses it again before anything is sent: over 127 bytes
//! marks the entry as an error, and an empty one leaves Rename insensitive,
//! as does the name the pods have already.
//! Nothing is cut by bytes: a name over 127 bytes in fewer characters is
//! marked, not cut. `max-length` stops a text at 127 characters, and cuts a
//! longer paste to them.

use adw::prelude::*;
use gtk::glib;

use crate::aap::name::{self, NameError};

const CANCEL: &str = "cancel";
const RENAME: &str = "rename";

/// The dialog's body: when GNOME Settings shows the new name, which is never
/// while an alias is set there, and that Apple devices keep their own name -
/// an iPhone did not follow a rename made here. The window's Name row shows
/// the new name at once, alias or not; an alias set in GNOME Settings stays
/// on show there, on the rest of the desktop, and in the switcher and the
/// notifications - see `Snapshot::display_name`.
pub(super) fn body(alias_set: bool) -> &'static str {
    if alias_set {
        "Settings keeps showing the name you gave the AirPods there. \
         Apple devices keep the name they have."
    } else {
        "Settings shows the new name once the AirPods connect again. \
         Apple devices keep the name they have."
    }
}

/// The line under the entry for a name the pods would not take whole.
pub(super) const TOO_LONG: &str = "The name is too long";

/// What the dialog does with `text` as typed, the pods being called
/// `current`: whether Rename takes it, and whether the entry is marked as an
/// error. Only a name too long is an error; an empty one, or the name the
/// pods have, simply cannot be renamed to.
pub(super) fn verdict(text: &str, current: &str) -> (bool, bool) {
    match name::check(text) {
        Ok(()) => (text != current, false),
        Err(NameError::TooLong(_)) => (false, true),
        Err(NameError::Empty | NameError::Nul) => (false, false),
    }
}

/// Opens the dialog over `parent`, the entry holding `current`, the pods'
/// name, with the cursor at its end. `rename` gets the new name when the
/// user renames; Cancel, Escape and closing the dialog do nothing.
pub(super) fn present(
    parent: &impl IsA<gtk::Widget>,
    current: &str,
    alias_set: bool,
    rename: impl Fn(String) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .name("rename-dialog")
        .heading("Rename AirPods")
        .body(body(alias_set))
        .close_response(CANCEL)
        .default_response(RENAME)
        .build();
    dialog.add_responses(&[(CANCEL, "_Cancel"), (RENAME, "_Rename")]);
    dialog.set_response_appearance(RENAME, adw::ResponseAppearance::Suggested);

    // Typing stops at 127 characters, and a longer paste is cut to them,
    // exact for ASCII; beyond ASCII a shorter name can pass 127 bytes, and
    // `verdict` marks it.
    let entry = gtk::Entry::builder()
        .name("rename-entry")
        .text(current)
        .max_length(name::MAX_NAME_LEN as i32)
        .activates_default(true)
        .secondary_icon_name("edit-clear-symbolic")
        .secondary_icon_tooltip_text("Clear")
        .build();
    entry.connect_icon_press(|entry, position| {
        if position == gtk::EntryIconPosition::Secondary {
            entry.set_text("");
            entry.grab_focus_without_selecting();
        }
    });
    let error = gtk::Label::builder()
        .name("rename-error")
        .label(TOO_LONG)
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .build();
    error.add_css_class("error");
    error.add_css_class("caption");
    let extra = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    extra.append(&entry);
    extra.append(&error);
    dialog.set_extra_child(Some(&extra));

    // Weak, all of them: the dialog holds the entry and the line, and the
    // entry its handlers, so a strong reference back would keep the dialog
    // alive for good.
    let current = current.to_string();
    let check = glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |entry: &gtk::Entry| {
            let (allowed, too_long) = verdict(&entry.text(), &current);
            dialog.set_response_enabled(RENAME, allowed);
            if too_long {
                entry.add_css_class("error");
            } else {
                entry.remove_css_class("error");
            }
            error.set_visible(too_long);
        }
    );
    check(&entry);
    entry.connect_changed(check);

    // Enter in the entry activates the default response, Rename, which does
    // nothing while it is insensitive.
    dialog.connect_response(
        Some(RENAME),
        glib::clone!(
            #[weak]
            entry,
            move |_, _| rename(entry.text().to_string())
        ),
    );

    // Focused as the dialog opens, which selects the whole name, as an
    // entry taking focus does; the cursor goes to the end instead, once the
    // focus is in, the first time only.
    let focus = gtk::EventControllerFocus::new();
    let first = std::cell::Cell::new(true);
    focus.connect_enter(glib::clone!(
        #[weak]
        entry,
        move |_| {
            if first.replace(false) {
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    entry,
                    move || entry.set_position(-1)
                ));
            }
        }
    ));
    entry.add_controller(focus);
    dialog.set_focus(Some(&entry));
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rename takes any name the pods keep whole; one over 127 bytes is
    /// marked, counted in bytes; an empty one is not taken, nor marked.
    #[test]
    fn the_dialog_takes_what_the_pods_keep() {
        let current = "Fake AirPods";
        assert_eq!(verdict("Test Pods", current), (true, false));
        assert_eq!(verdict(&"a".repeat(127), current), (true, false));
        assert_eq!(verdict(&"a".repeat(128), current), (false, true));
        // 43 characters, under max-length, and 129 bytes.
        assert_eq!(verdict(&"’".repeat(43), current), (false, true));
        assert_eq!(verdict("", current), (false, false));
    }

    /// The name the pods have already is no rename: not taken, nor marked.
    #[test]
    fn the_dialog_takes_no_rename_to_the_same_name() {
        assert_eq!(verdict("Test Pods", "Test Pods"), (false, false));
        assert_eq!(verdict("Test Pods ", "Test Pods"), (true, false));
    }

    #[test]
    fn the_body_says_where_the_name_shows() {
        assert!(
            body(false).starts_with("Settings shows the new name once the AirPods connect again.")
        );
        assert!(
            body(true).starts_with("Settings keeps showing the name you gave the AirPods there.")
        );
        for alias_set in [false, true] {
            assert!(body(alias_set).ends_with("Apple devices keep the name they have."));
        }
    }
}
