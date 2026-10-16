//! Test harness: fakes that stand in for AirPods, so the app can run and be
//! tested without them (#74).
//!
//! Built only with the `harness` feature, which `lib.rs` refuses outside debug
//! builds.

pub mod pods;

pub use pods::Pods;
