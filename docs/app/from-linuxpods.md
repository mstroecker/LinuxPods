# From LinuxPods

What Cymba does with what LinuxPods left behind. Up to 0.5.0 the app was named
LinuxPods: it kept its files under that name, and its installer left a binary and
desktop entries under it. The code is `migrate` and `scripts/install.sh`. The move has
run only in tests and against a throwaway home so far: see [Hardware checks](#hardware-checks).

## The directories

At startup, `migrate::from_linuxpods` moves each LinuxPods directory to Cymba's:

| LinuxPods | Cymba | Holds |
|---|---|---|
| `~/.local/share/linuxpods` | `~/.local/share/cymba` | `keys.json`, `readings.json` ([keystore.md](keystore.md), [readings.md](readings.md)) |
| `~/.config/linuxpods` | `~/.config/cymba` | `settings.json` ([ui.md](ui.md#settings)) |

The paths follow `$XDG_DATA_HOME` and `$XDG_CONFIG_HOME`.

Each directory moves in one rename. The two names sit side by side in one parent, so the
rename never crosses a filesystem. The files keep their modes, and a move is never left
half done.

| What is under Cymba's name | What happens |
|---|---|
| Nothing | The LinuxPods directory moves. |
| An empty directory | The LinuxPods directory moves, in its place. |
| A directory with files in it, or anything else | Both stay as they are, and a warning is logged. |
| No LinuxPods directory | Nothing happens, and nothing is made. |

A move that fails is logged, and the app starts without the files:

- The keys come back with the pair's next AAP connection, which requests them every time
  ([keystore.md](keystore.md#requested-on-every-connection)). Until then the pair's
  advertisements are dropped, so a pair in its case shows no battery.
- The last readings are gone.
- The switches in Preferences start at their defaults.

## When

The move runs at the start of the app's startup handler, before the coordinator loads
the keys and the readings and before the settings are opened. GApplication runs that
handler only in the primary instance, so a second launch moves nothing
([protocol/linux/desktop.md](../protocol/linux/desktop.md#gapplication)).

A LinuxPods still running is another application to GApplication: its ID differs, so
nothing stops Cymba from starting beside it
([protocol/linux/desktop.md](../protocol/linux/desktop.md#gapplication)). It would talk
to the same pair, and recreate its directory at its next save. The installer stops it
first.

The move is one way. A LinuxPods started again afterwards finds no directory of its own:
it starts with no keys, no readings and the default settings.

## The installer

`scripts/install.sh` removes a LinuxPods install, whether it installs Cymba or uninstalls
it:

- it stops `~/.local/bin/linuxpods` and waits until it has exited, so it is not saving
  into its directories when Cymba moves them: SIGTERM, which quits it as its menu does,
  then SIGKILL after 5 seconds; it gives up, and installs nothing, if the process is
  still there 2 seconds later
- it removes that binary, and the icons, launcher entry and autostart entry under both
  earlier app IDs: `io.github.mstroecker.LinuxPods`, and `com.linuxpods.app` before it

It starts Cymba after that, and Cymba's startup moves the directories.

## The fake world

`fakepods --keystore` refuses the LinuxPods data directory as it refuses Cymba's.
Fake keys saved there would be moved over the user's at the app's next start.

## Hardware checks

- On a machine with a LinuxPods install and keys: quit LinuxPods, start Cymba, and check
  that `~/.local/share/cymba` holds the keys and that a pair in range shows its battery
  over BLE before any AAP connection.
- Run `install.sh` from a release tarball over a running LinuxPods install, and check that
  one instance is left, Cymba's.
- GNOME keeps a notification choice per app: check whether one made for LinuxPods in
  GNOME Settings > Notifications carries over to Cymba. The guess is that it does not, as
  the app ID differs.
