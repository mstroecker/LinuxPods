//! LinuxPods: AirPods management for GNOME.
//!
//! Split into a library so the protocol and coordination layers can be driven
//! from integration tests and probes, not just the GUI binary.

// Several ported helpers mirror the Go API surface but are not wired up yet
// (key/colour decoding, battery removal). Kept deliberately.
#![allow(dead_code)]

// The harness redirects Bluetooth traffic to a fake and lets another process
// press the UI's controls; none of that may reach a release build.
#[cfg(all(feature = "harness", not(debug_assertions)))]
compile_error!("the `harness` feature is for debug builds only - build without --release");

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
