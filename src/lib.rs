//! LinuxPods: AirPods management for GNOME.
//!
//! Split into a library so the protocol and coordination layers can be driven
//! from integration tests and probes, not just the GUI binary.

// The harness redirects Bluetooth traffic to a fake and lets another process
// press the UI's controls; none of that may reach a release build.
#[cfg(all(feature = "harness", not(debug_assertions)))]
compile_error!("the `harness` feature is for debug builds only - build without --release");

/// The length of a proximity key, AES-128: the ENC_KEY that AAP hands over
/// (`aap::keys`) and BLE decrypts advertisements with (`ble::decrypt`). Here
/// rather than in either, so neither layer depends on the other for it.
pub const KEY_LEN: usize = 16;

/// The application ID: the name the app owns on the session bus, and the one
/// its icon and desktop file are installed under.
pub const APP_ID: &str = "io.github.mstroecker.LinuxPods";

/// [`APP_ID`]'s icon as a symbolic: the tray's, which the panel recolors to
/// match its own foreground, and the window's while it has no device to show.
pub const APP_SYMBOLIC_ICON: &str = "io.github.mstroecker.LinuxPods-symbolic";

/// [`APP_ID`] as an object path: where GTK looks for the app's resources, and
/// the namespace of the objects it serves on D-Bus.
pub const APP_PATH: &str = "/io/github/mstroecker/LinuxPods";

/// Bytes as hex, two lowercase digits each and a space between: `04 00 04 00`.
/// For logs and probe output, which get pasted into issues - a key's bytes go
/// through `aap::keys::redacted_hex` instead.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub mod aap;
pub mod attachment;
pub mod ble;
pub mod bluez;
#[cfg(feature = "harness")]
pub mod harness;
pub mod indicator;
pub mod keystore;
pub mod podstate;
pub mod ui;

#[cfg(test)]
mod tests {
    use super::*;

    /// Spelled out in full, as `concat!` cannot build on a `const`, and so
    /// held to the ID here.
    #[test]
    fn the_icon_and_path_follow_the_app_id() {
        assert_eq!(APP_SYMBOLIC_ICON, format!("{APP_ID}-symbolic"));
        assert_eq!(APP_PATH, format!("/{}", APP_ID.replace('.', "/")));
    }

    #[test]
    fn hex_spaces_its_bytes() {
        assert_eq!(hex(&[0x04, 0x00, 0xab]), "04 00 ab");
        assert_eq!(hex(&[]), "");
    }
}
