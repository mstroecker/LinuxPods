//! fakepods: a fake world of AirPods for LinuxPods to run against (#72).
//!
//! Serves a fake BlueZ and `FakeWorld1` on the bus at `--address`, with a
//! pair's AAP socket in `--socket-dir`, and runs until killed.
//! `scripts/fake-world.sh` starts it on a private bus with the app beside it,
//! and `make fake-world` builds both first. See `harness::world` for what the
//! world does and how to change it.
//!
//! The scenario sets the world up before it takes its names, so the app finds
//! it complete. With `--keystore`, each of its pairs' keys is saved there as
//! the app keeps keys - as though each had connected to the app before. The
//! app reads nothing from a pair whose key it lacks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::Address;
use linuxpods::harness::World;
use linuxpods::harness::pods::{PRO_2_USB_C, PRO_3};
use linuxpods::keystore::Keystore;

const USAGE: &str = "\
usage: fakepods --address ADDRESS --socket-dir DIR [--scenario NAME] [--keystore DIR]

  --address ADDRESS  the bus to serve on - a private one, never the system bus
  --socket-dir DIR   where each pair's AAP socket goes: LINUXPODS_AAP_SOCKET_DIR
  --scenario NAME    in-range (default), connected, two-pairs, flood or empty
  --keystore DIR     save the pairs' keys here, as the app's data directory has them

Scenarios:
  in-range   one pair in range, disconnected
  connected  one pair connected
  two-pairs  one pair connected, another in range
  flood      one pair in range among twenty new undecryptable advertisers a second
  empty      nothing: add pairs with FakeWorld1.AddPods";

/// The scenarios' pairs, from the block RFC 7042 sets aside for
/// documentation, so they are nobody's.
const FIRST: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
const SECOND: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

/// How many advertisers the flood scenario adds a second.
const FLOOD_RATE: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    InRange,
    Connected,
    TwoPairs,
    Flood,
    Empty,
}

/// The scenarios by the names the command line gives them.
const SCENARIOS: [(&str, Scenario); 5] = [
    ("in-range", Scenario::InRange),
    ("connected", Scenario::Connected),
    ("two-pairs", Scenario::TwoPairs),
    ("flood", Scenario::Flood),
    ("empty", Scenario::Empty),
];

impl Scenario {
    fn parse(name: &str) -> Option<Self> {
        SCENARIOS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
    }

    fn name(self) -> &'static str {
        SCENARIOS
            .iter()
            .find(|(_, s)| *s == self)
            .map(|(n, _)| *n)
            .expect("every scenario is named")
    }

    /// Puts the scenario's pairs in the world.
    async fn set_up(self, world: &World) -> Result<()> {
        if self != Self::Empty {
            world.add_pods(FIRST, PRO_3, "AirPods Pro 3 (fake)").await?;
        }
        if matches!(self, Self::Connected | Self::TwoPairs) {
            world.connect(FIRST).await?;
        }
        if self == Self::TwoPairs {
            world
                .add_pods(SECOND, PRO_2_USB_C, "AirPods Pro 2 (fake)")
                .await?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
struct Args {
    address: String,
    socket_dir: PathBuf,
    scenario: Scenario,
    keystore: Option<PathBuf>,
}

/// `None` for `--help`.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let (mut address, mut socket_dir, mut keystore) = (None, None, None);
    let mut scenario = Scenario::InRange;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        let value = args.next().ok_or_else(|| format!("{arg} takes a value"))?;
        match arg.as_str() {
            "--address" => address = Some(value),
            "--socket-dir" => socket_dir = Some(PathBuf::from(value)),
            "--keystore" => keystore = Some(PathBuf::from(value)),
            "--scenario" => {
                scenario =
                    Scenario::parse(&value).ok_or_else(|| format!("no scenario {value:?}"))?
            }
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    let address = address.ok_or("--address is required")?;
    // Taking org.bluez there is what the private bus exists to prevent.
    if address.contains("/run/dbus/system_bus_socket") {
        return Err("that is the system bus; give a private one".into());
    }
    Ok(Some(Args {
        address,
        socket_dir: socket_dir.ok_or("--socket-dir is required")?,
        scenario,
        keystore,
    }))
}

async fn run(args: Args) -> Result<()> {
    let world = World::new(&args.address, &args.socket_dir).await?;
    args.scenario.set_up(&world).await?;
    let macs = world.macs().await;
    if let Some(dir) = &args.keystore {
        let mut keys = HashMap::new();
        for &mac in &macs {
            keys.insert(mac, world.keys(mac).await?.enc_key.to_vec());
        }
        Keystore::with_dir(dir.clone())
            .and_then(|keystore| keystore.save(&keys))
            .with_context(|| format!("failed to save the keys in {}", dir.display()))?;
    }
    world.publish().await?;
    tracing::info!(
        "{} ready on {} with {} pair(s)",
        args.scenario.name(),
        args.address,
        macs.len()
    );

    if args.scenario == Scenario::Flood {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            world.flood(FLOOD_RATE).await?;
        }
    }
    std::future::pending().await
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=info,fakepods=info".into()),
        )
        .init();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("fakepods: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Args>, String> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn takes_its_options_in_any_order() {
        let args = parse(&[
            "--scenario",
            "two-pairs",
            "--socket-dir",
            "/run/user/1000/aap",
            "--address",
            "unix:path=/run/user/1000/bus",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(args.scenario, Scenario::TwoPairs);
        assert_eq!(args.socket_dir, PathBuf::from("/run/user/1000/aap"));
        assert_eq!(args.keystore, None);
        assert_eq!(parse(&["--help"]), Ok(None));
    }

    #[test]
    fn scenarios_go_by_their_names() {
        for (name, scenario) in SCENARIOS {
            assert_eq!(Scenario::parse(name), Some(scenario));
            assert_eq!(scenario.name(), name);
            assert!(USAGE.contains(name), "{name} is in the usage");
        }
        assert_eq!(Scenario::parse("InRange"), None);
    }

    #[test]
    fn needs_an_address_and_a_socket_dir() {
        assert!(parse(&["--socket-dir", "/tmp/aap"]).is_err());
        assert!(parse(&["--address", "unix:path=/tmp/bus"]).is_err());
        assert!(parse(&["--address"]).is_err());
        let scenario = [
            "--address",
            "unix:path=/tmp/b",
            "--socket-dir",
            "/tmp/a",
            "--scenario",
        ];
        assert!(parse(&[&scenario[..], &["moon"]].concat()).is_err());
    }

    /// The real bluetoothd is there.
    #[test]
    fn refuses_the_system_bus() {
        for address in [
            "unix:path=/run/dbus/system_bus_socket",
            "unix:path=/var/run/dbus/system_bus_socket",
        ] {
            let err = parse(&["--address", address, "--socket-dir", "/tmp/aap"]).unwrap_err();
            assert!(err.contains("system bus"), "{err}");
        }
    }
}
