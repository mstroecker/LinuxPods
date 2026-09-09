//! LinuxPods: AirPods management for GNOME.
//!
//! Split into a library so the protocol and coordination layers can be driven
//! from integration tests and probes, not just the GUI binary.

// Several ported helpers mirror the Go API surface but are not wired up yet
// (noise control, key/colour decoding, battery removal). Kept deliberately.
#![allow(dead_code)]

pub mod aap;
pub mod ble;
pub mod bluez;
pub mod indicator;
pub mod keystore;
pub mod podstate;
pub mod ui;
