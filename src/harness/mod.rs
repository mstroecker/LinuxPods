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
    bus_config_with(dir, None)
}

/// [`bus_config`], activating what `services` has service files for.
fn bus_config_with(dir: &std::path::Path, services: Option<&std::path::Path>) -> String {
    let servicedir = services
        .map(|s| format!("  <servicedir>{}</servicedir>\n", s.display()))
        .unwrap_or_default();
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
{servicedir}  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        dir.display()
    )
}

/// Where an activating bus records each time it was asked, in its directory:
/// the pid of what it started, a line each - see [`PrivateBus::activating`].
const ACTIVATIONS: &str = "activations";

/// The variable that tells what an activating bus starts where to record it,
/// so that the path stays out of the service file's `Exec` and its quoting.
const ACTIVATIONS_VAR: &str = "LINUXPODS_BUS_ACTIVATIONS";

/// A `dbus-daemon` of its own, for a test to run the fake world and the app's
/// D-Bus code on, out of reach of the real system and session buses. Stopped
/// when dropped, with whatever it activated.
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
        Self::start_with(None)
    }

    /// [`PrivateBus::start`], on a bus that activates `name`, as systemd has
    /// bluetoothd activated: a call to `name` while nobody owns it waits until
    /// somebody takes it, as it would while systemd starts the service.
    /// Nothing is started for it: what the bus runs only records that it was
    /// asked - see [`PrivateBus::activations`] - and waits, while the test
    /// takes the name itself.
    pub fn activating(name: &str) -> Result<Self> {
        Self::start_with(Some(name))
    }

    /// How many times the bus was asked to start the name it activates.
    pub fn activations(&self) -> usize {
        std::fs::read_to_string(self._dir.path().join(ACTIVATIONS))
            .map(|asked| asked.lines().count())
            .unwrap_or(0)
    }

    fn start_with(activated: Option<&str>) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("linuxpods-bus")
            .tempdir()
            .context("failed to create a directory for the bus")?;
        let services = match activated {
            Some(name) => {
                let services = dir.path().join("services");
                std::fs::create_dir(&services)?;
                // Kept running until the name is taken, as activation fails
                // once what it started has exited, and killed with the bus.
                // Its output goes nowhere: kept, it would hold the test's
                // stderr open.
                let service = format!(
                    "[D-BUS Service]\nName={name}\nExec=/bin/sh -c '\
                     echo $$ >> \"${ACTIVATIONS_VAR}\"; \
                     exec sleep 20 </dev/null >/dev/null 2>&1'\n"
                );
                std::fs::write(services.join(format!("{name}.service")), service)?;
                Some(services)
            }
            None => None,
        };
        let config = dir.path().join("bus.conf");
        std::fs::write(&config, bus_config_with(dir.path(), services.as_deref()))?;

        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .env(ACTIVATIONS_VAR, dir.path().join(ACTIVATIONS))
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

    /// A new connection to the bus to serve battery providers on, as
    /// [`crate::bluez::connection`] builds one: what attachment runs on.
    pub async fn connect_for_providers(&self) -> Result<crate::bluez::ProviderConnection> {
        crate::bluez::connection(zbus::connection::Builder::address(self.address())?)
            .await
            .with_context(|| format!("failed to connect to {}", self.address))
    }
}

/// A connection of the fake world's to the bus at `bus`, to serve objects
/// on: the fake BlueZ's, or the world's own control. It serves an
/// ObjectManager at `/` from the start - the fake BlueZ's, as bluetoothd
/// serves one there, and on the control connection there only for this - so
/// that its object server answers from the first message. On a connection
/// built plain, zbus starts dispatching method calls in a task the first
/// `object_server()` spawns, and drops a method call that comes before that
/// task has subscribed, as the app's battery provider did (#125).
pub(crate) async fn connect_serving(bus: &str) -> Result<zbus::Connection> {
    zbus::connection::Builder::address(bus)?
        .serve_at("/", zbus::fdo::ObjectManager)?
        .build()
        .await
        .with_context(|| format!("failed to connect to {bus}"))
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // What it activated would outlive it.
        let started = std::fs::read_to_string(self._dir.path().join(ACTIVATIONS));
        for pid in started.iter().flat_map(|pids| pids.split_whitespace()) {
            let _ = Command::new("kill")
                .arg(pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
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
