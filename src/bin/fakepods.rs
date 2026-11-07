//! fakepods: a fake world of AirPods for Cymba to run against (#72).
//!
//! Serves a fake BlueZ and `FakeWorld1` on the bus at `--address`, with a
//! pair's AAP socket in `--socket-dir`, and runs until killed. The fake BlueZ
//! has one adapter, `hci0` unless `--adapter` names another, as on a machine
//! whose only adapter is not the first (#51). It runs as a stock bluetoothd
//! does, without its experimental interfaces, so the app discovers; with
//! `--experimental` it offers advertisement monitors, and the app listens
//! through one (#189).
//! `scripts/fake-world.sh` starts it on a private bus with the app beside it,
//! and `make fake-world` builds both first. See `harness::world` for what the
//! world does and how to change it.
//!
//! The scenario sets the world up before it takes its names, so the app finds
//! it complete. With `--keystore`, each of its pairs' keys is saved there as
//! the app keeps keys - as though each had connected to the app before. The
//! app reads nothing from a pair whose key it lacks.
//!
//! With `--player-address`, a fake media player - see `harness::player` -
//! takes `org.mpris.MediaPlayer2.fakeplayer` on that bus, the app's session
//! bus, for the app to pause and play as a pod leaves an ear (#167). It
//! refuses the user's own session bus.
//!
//! The save replaces the key file outright, so `--keystore` refuses the app's
//! own data directory, the LinuxPods one the app moves there at startup, and
//! any directory with a key file in it already. Saved over the user's keys,
//! the fake ones would leave the app reading nothing from a real pair over BLE
//! until that pair reconnected over AAP.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::Address;
use cymba::harness::pods::{PRO_2_USB_C, PRO_3};
use cymba::harness::world::DEFAULT_ADAPTER;
use cymba::harness::{FakePlayer, World};
use cymba::keystore::{self, KEYS_FILE, Keystore, PairKeys};
use cymba::migrate;

const USAGE: &str = "\
usage: fakepods --address ADDRESS --socket-dir DIR [--scenario NAME] [--adapter NAME]
                [--experimental] [--keystore DIR] [--player-address ADDRESS]

  --address ADDRESS  the bus to serve on - a private one, never the system bus
  --socket-dir DIR   where each pair's AAP socket goes: CYMBA_AAP_SOCKET_DIR
  --scenario NAME    in-range (default), connected, two-pairs, flood or empty
  --adapter NAME     the fake BlueZ's one adapter, at /org/bluez/NAME: hci0 (default),
                     or another such as hci1
  --experimental     run the fake BlueZ with its experimental interfaces on, offering
                     advertisement monitors, as Experimental = true in main.conf does
  --keystore DIR     save the pairs' keys in DIR/keys.json for the fake world's app;
                     never the app's own data directory or its LinuxPods one,
                     nor a DIR holding keys
  --player-address ADDRESS
                     serve a fake media player, org.mpris.MediaPlayer2.fakeplayer,
                     on the app's session bus - a private one, never the user's

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

/// The fake media player's name, after `org.mpris.MediaPlayer2.`.
const PLAYER: &str = "fakeplayer";

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
    adapter: String,
    experimental: bool,
    keystore: Option<PathBuf>,
    player_address: Option<String>,
}

/// `None` for `--help`. `app_dirs` are where the app keeps its keys, which
/// `--keystore` must not be.
fn parse_args(
    mut args: impl Iterator<Item = String>,
    app_dirs: &[PathBuf],
) -> Result<Option<Args>, String> {
    let (mut address, mut socket_dir, mut keystore, mut player_address) = (None, None, None, None);
    let mut scenario = Scenario::InRange;
    let mut adapter = DEFAULT_ADAPTER.to_string();
    let mut experimental = false;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        if arg == "--experimental" {
            experimental = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{arg} takes a value"))?;
        match arg.as_str() {
            "--address" => address = Some(value),
            "--socket-dir" => socket_dir = Some(PathBuf::from(value)),
            "--adapter" => adapter = value,
            "--keystore" => keystore = Some(PathBuf::from(value)),
            "--player-address" => player_address = Some(value),
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
    if let Some(dir) = &keystore {
        check_keystore(dir, app_dirs)?;
    }
    // The user's players are there, and a fake one among them would be
    // paused and played by their own Cymba.
    if player_address.as_deref().is_some_and(is_user_session_bus) {
        return Err("--player-address is the user's session bus; give a private one".into());
    }
    Ok(Some(Args {
        address,
        socket_dir: socket_dir.ok_or("--socket-dir is required")?,
        scenario,
        adapter,
        experimental,
        keystore,
        player_address,
    }))
}

/// Whether `address` names the user's session bus where systemd puts it,
/// `/run/user/<uid>/bus`.
fn is_user_session_bus(address: &str) -> bool {
    address
        .split([';', ','])
        .filter_map(|part| part.split_once("path="))
        .any(|(_, path)| {
            path.strip_prefix("/run/user/")
                .and_then(|rest| rest.strip_suffix("/bus"))
                .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Refuses a `--keystore` whose save would replace keys this run did not
/// make: one of `app_dirs` however `dir` spells it, or a directory holding a
/// key file already - the run has saved nothing yet, so that one is someone
/// else's.
fn check_keystore(dir: &Path, app_dirs: &[PathBuf]) -> Result<(), String> {
    let resolved = resolve(dir).map_err(|e| format!("--keystore {}: {e}", dir.display()))?;
    if app_dirs
        .iter()
        .any(|app| resolve(app).is_ok_and(|app| app == resolved))
    {
        return Err("that is the app's own data directory; give a new one".into());
    }
    if std::fs::symlink_metadata(resolved.join(KEYS_FILE)).is_ok() {
        return Err(format!(
            "{} holds a {KEYS_FILE} already; give a new directory",
            dir.display()
        ));
    }
    Ok(())
}

/// `path` made absolute, with every link in it followed - where the save
/// lands once `Keystore::with_dir` has made what is missing. Each part is
/// resolved as soon as it exists, so a link reached through a `..` after a
/// part that does not exist yet is followed too.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for part in std::path::absolute(path)?.components() {
        match part {
            // Nothing resolved so far is a link, so this is the real parent.
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            part => resolved.push(part),
        }
        if let Ok(real) = resolved.canonicalize() {
            resolved = real;
        }
    }
    Ok(resolved)
}

async fn run(args: Args) -> Result<()> {
    let adapters = [(args.adapter.as_str(), true)];
    let world = World::with_adapters(&args.address, &args.socket_dir, &adapters).await?;
    // Before the name is taken, as bluetoothd reads it as it starts.
    world.bluez().set_experimental(args.experimental).await?;
    args.scenario.set_up(&world).await?;
    let macs = world.macs().await;
    if let Some(dir) = &args.keystore {
        let mut keys = HashMap::new();
        for &mac in &macs {
            let pair = world.keys(mac).await?;
            keys.insert(mac, PairKeys::new(pair.enc_key, Some(pair.irk)));
        }
        Keystore::with_dir(dir.clone())
            .and_then(|keystore| keystore.save(&keys))
            .with_context(|| format!("failed to save the keys in {}", dir.display()))?;
    }
    world.publish().await?;
    // Kept until the process ends, which takes it off the bus.
    let _player = match &args.player_address {
        Some(address) => Some(FakePlayer::serve(address, PLAYER).await?),
        None => None,
    };
    tracing::info!(
        "{} ready on {} with {} pair(s) on {}",
        args.scenario.name(),
        args.address,
        macs.len(),
        args.adapter
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
                .unwrap_or_else(|_| "cymba=info,fakepods=info".into()),
        )
        .init();

    // The LinuxPods directory too: the app moves it to its own at startup.
    let app_dirs: Vec<PathBuf> = keystore::default_dir()
        .map(|dir| vec![migrate::linuxpods_dir(&dir), dir])
        .unwrap_or_default();
    let args = match parse_args(std::env::args().skip(1), &app_dirs) {
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
        parse_args(args.iter().map(|a| a.to_string()), &[])
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
        assert_eq!(args.adapter, "hci0");
        assert!(!args.experimental);
        assert_eq!(args.keystore, None);
        assert_eq!(args.player_address, None);
        assert_eq!(parse(&["--help"]), Ok(None));

        let args = parse(&[
            "--adapter",
            "hci1",
            "--experimental",
            "--address",
            "unix:path=/tmp/b",
            "--socket-dir",
            "/tmp/a",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(args.adapter, "hci1");
        assert!(args.experimental, "a flag, taking no value");

        let args = parse(&[
            "--address",
            "unix:path=/tmp/b",
            "--socket-dir",
            "/tmp/a",
            "--player-address",
            "unix:path=/run/user/1000/cymba-harness/world/session/dbus-x,guid=1",
        ])
        .unwrap()
        .unwrap();
        assert!(args.player_address.is_some());
    }

    /// The user's own players are there.
    #[test]
    fn refuses_the_users_session_bus_for_the_player() {
        for address in [
            "unix:path=/run/user/1000/bus",
            "unix:path=/run/user/1000/bus,guid=0123",
            "unix:abstract=/tmp/x;unix:path=/run/user/42/bus",
        ] {
            let err = parse(&[
                "--address",
                "unix:path=/tmp/b",
                "--socket-dir",
                "/tmp/a",
                "--player-address",
                address,
            ])
            .unwrap_err();
            assert!(err.contains("session bus"), "{address}: {err}");
        }
        assert!(!is_user_session_bus("unix:path=/run/user/1000/busy"));
        assert!(!is_user_session_bus("unix:path=/run/user//bus"));
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

    /// The user's keys are there, or will be once the app moves the LinuxPods
    /// directory: the save would replace them with the fake pairs'. Refused
    /// however the path spells it, and before the app has made the directory.
    #[test]
    fn refuses_the_apps_own_keystore() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        // Not made yet, and named through the link.
        let app_dirs = [link.join("cymba"), link.join("linuxpods")];
        let with_keystore = |dir: &Path| {
            let args = [
                "--address",
                "unix:path=/tmp/b",
                "--socket-dir",
                "/tmp/a",
                "--keystore",
                dir.to_str().unwrap(),
            ];
            parse_args(args.into_iter().map(String::from), &app_dirs)
        };

        // From the working directory, which the tests do not change.
        let up: PathBuf = std::env::current_dir()
            .unwrap()
            .components()
            .skip(1)
            .map(|_| "..")
            .collect();
        for name in ["cymba", "linuxpods"] {
            for dir in [
                link.join(name),
                data.join(name),
                data.join(format!("./{name}/")),
                data.join(format!("new/../{name}")),
                up.join(data.join(name).strip_prefix("/").unwrap()),
                // Back out of a part that does not exist, and through the link.
                home.path().join(format!("new/../link/{name}")),
            ] {
                let err = with_keystore(&dir).unwrap_err();
                assert!(
                    err.contains("own data directory"),
                    "{}: {err}",
                    dir.display()
                );
            }
        }

        // Anywhere else, so long as no key file is there.
        let other = home.path().join("other");
        assert!(with_keystore(&other).is_ok(), "a new directory");
        std::fs::create_dir(&other).unwrap();
        assert!(with_keystore(&other).is_ok(), "an empty one");
        std::fs::write(other.join(KEYS_FILE), "{}").unwrap();
        for dir in [other.clone(), home.path().join("new/../other")] {
            let err = with_keystore(&dir).unwrap_err();
            assert!(err.contains("keys.json"), "{}: {err}", dir.display());
        }
    }
}
