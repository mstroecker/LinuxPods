//! Test harness: fakes that stand in for AirPods, so the app can run and be
//! tested without them (#74).
//!
//! - [`pods`]: one pair of fake AirPods, pure state in and bytes out.
//! - [`bluez`]: a fake `org.bluez`, with as many adapters as asked for, for
//!   the app's scanner and battery provider.
//! - [`world`]: pairs, the fake BlueZ and their AAP sockets running together,
//!   changed over D-Bus or from the same process. `fakepods` runs one.
//!
//! Built only with the `harness` feature, which `lib.rs` refuses outside debug
//! builds.

pub mod bluez;
pub mod pods;
pub mod world;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

pub use bluez::FakeBluez;
pub use pods::Pods;
pub use world::World;

/// A bus configuration for a private `dbus-daemon` listening in `dir`: a
/// session-type bus open to every connection of this user, with no service
/// directory, so nothing is ever activated on it - no portal, dconf or
/// accessibility bus. `scripts/fake-world.sh` writes the same.
pub fn bus_config(dir: &std::path::Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        dir.display()
    )
}

/// A `dbus-daemon` of its own, for a test to run the fake world and the app's
/// D-Bus code on, out of reach of the real system and session buses. Stopped
/// when dropped.
///
/// Its socket goes in a new directory under the temporary directory: a Unix
/// socket path longer than 108 bytes cannot be bound, so a long `TMPDIR` breaks
/// this, as it does the AAP sockets.
pub struct PrivateBus {
    daemon: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// Starts the daemon and waits until it listens.
    pub fn start() -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("linuxpods-bus")
            .tempdir()
            .context("failed to create a directory for the bus")?;
        let config = dir.path().join("bus.conf");
        std::fs::write(&config, bus_config(dir.path()))?;

        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to start dbus-daemon")?;

        // The address is printed once the socket listens.
        let mut address = String::new();
        let stdout = daemon.stdout.take().expect("stdout is piped");
        let read = BufReader::new(stdout).read_line(&mut address);
        let address = address.trim().to_string();
        if read.is_err() || address.is_empty() {
            let _ = daemon.kill();
            let _ = daemon.wait();
            anyhow::bail!("dbus-daemon printed no address");
        }
        Ok(Self {
            daemon,
            address,
            _dir: dir,
        })
    }

    /// The address to connect to, as `DBUS_*_BUS_ADDRESS` would give it.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// A new connection to the bus.
    pub async fn connect(&self) -> Result<zbus::Connection> {
        zbus::connection::Builder::address(self.address())?
            .build()
            .await
            .with_context(|| format!("failed to connect to {}", self.address))
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing can be activated on the bus, and it is not the user's.
    #[tokio::test]
    async fn a_private_bus_activates_nothing() {
        let bus = PrivateBus::start().unwrap();
        assert_ne!(
            Some(bus.address()),
            std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref()
        );
        let conn = bus.connect().await.unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        let activatable = dbus.list_activatable_names().await.unwrap();
        assert_eq!(activatable.len(), 1, "{activatable:?}");
        assert_eq!(activatable[0].as_str(), "org.freedesktop.DBus");
    }
}
