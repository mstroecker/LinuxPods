# Names

How Cymba names a pair: the pods' own name beside BlueZ's alias, the Name row, the
rename dialog, and the name shown everywhere else. The rename packet and what the pods do
with a name are theirs: [protocol/aap/rename.md](../protocol/aap/rename.md). The alias is
BlueZ's: [protocol/linux/bluez.md](../protocol/linux/bluez.md#device1name-and-device1alias).
The rename packet ran on a Pro 3 (#160); the app's rename has run against the fake world
only.

## Two names

The pods' name and BlueZ's alias:
[protocol/aap/rename.md](../protocol/aap/rename.md#two-names),
[protocol/linux/bluez.md](../protocol/linux/bluez.md#device1name-and-device1alias).

**`Device1.Alias` is never set or cleared.**

- **Set on every rename,** it would pin the name, so that a later rename on the pods - from
  an iPhone, say - never showed here.
- **Cleared,** it would throw away a name the user gave in GNOME.
- **With no alias set,** the new name shows in GNOME at the next reconnect on its own.

Whether an alias is set the app reads as `Alias != Name`, from the names attachment reads
with `GetManagedObjects` on every connection BlueZ confirms (`bluez::DeviceNames`). An
alias set in GNOME while the pair stays connected is seen at its next connection event.

### The Name row

The Name row in the window's Info group shows the pods' name where the link has reported
it, else BlueZ's alias.

- **It opens the rename dialog only once the link has reported the pods' name.** Started
  from the alias, Enter would rename the pods to it.
- **The pods' name comes in the startup dump,** which on a new link waits for a second
  `0x0C` naming this host ([protocol/aap/session.md](../protocol/aap/session.md#initial-state-report)).
  Until then the row stays insensitive, on the alias.
- **The pods' name goes with the link** (`Link::name`). Off AAP it is not known, so a pair
  renamed and gone shows the old name until BlueZ reads the new one.

### Everywhere else

The switcher in the header bar and a low battery notification name a pair as the rest of
the desktop will once BlueZ has caught up (`Snapshot::display_name`):

1. an alias set in BlueZ;
2. else the pods' name, where the link has it;
3. else BlueZ's alias;
4. then the model.

Named by the alias alone, the switcher kept the old name after a rename until the pair
connected again: BlueZ's `Name` and `Alias` stayed old until then in the
[Pro 3 run](../protocol/aap/rename.md#hardware-run-pro-3-2026-11-02) (#160).

**An alias typed back to the pods' old name is the exception.** It reads as unset until
the pair connects again, so the switcher shows the pods' new name until then, and the alias
after. The dialog's body is wrong then too: it says Settings shows the new name once the
AirPods connect again, when Settings keeps the alias.

## Rename

The packet, and what the pods do with a name:
[protocol/aap/rename.md](../protocol/aap/rename.md#rename-0x1a).

- **Sent by `Command::Rename`,** from the Name row ([commands.md](commands.md)).
- **Fire and forget,** as nothing answers it: the name is recorded on the link
  (`Link::name`) once the packet is away.
- **Refused, never sent, never cut by bytes:** an empty name, a name with a NUL, and one
  over `aap::name::MAX_NAME_LEN` bytes. The pods would cut a longer one silently, at a
  byte, inside a character if it fell there. Refused in three places:
  - the dialog;
  - `Coordinator::command`, before the command is queued;
  - `aap::name::rename_packet`.
- **The dialog starts from the pods' name, never the alias,** and its Rename stays
  insensitive for the name the pods have. A failed rename is a toast.

### Long names

What the pods keep of a long name:
[protocol/aap/rename.md](../protocol/aap/rename.md#name-run-pro-3-2026-11-02).

Any name up to `MAX_NAME_LEN` bytes is allowed, whatever its characters. In the dialog, the
entry's `max-length` of 127 stops a text at 127 characters, and cuts a longer paste to them:
exact for ASCII. A shorter name with characters beyond ASCII can still pass `MAX_NAME_LEN`
bytes; it gets the error style and a line saying it is too long, with Rename insensitive.

### What the dialog says

What the run showed of BlueZ's `Name` and of Apple devices:
[protocol/aap/rename.md](../protocol/aap/rename.md#what-a-rename-does).

- **BlueZ's `Name` catches up at the next reconnect:** the dialog says so, and the app
  forces no reconnect.
- **Apple devices keep their own name:** the dialog says so too.

How the fake pods take a rename: [development/fake-world.md](../development/fake-world.md#rename).

## Hardware checks

- Rename from the window on the Pro 3: the name in the next session, and after a reconnect
  in GNOME.
- With an alias set in GNOME Settings: the dialog's other body, and the alias left as it
  was.
- A name over `MAX_NAME_LEN` bytes refused in the dialog; a failed send's toast.
- After a rename, the switcher shows the new name at once, and one reconnect shows it in
  GNOME, without a second reconnect.
- With an alias set in GNOME Settings, the switcher keeps the alias after a rename.
- The dialog on real GNOME: the focus, the cursor at the end, Enter, a long paste cut to
  127 characters.
- While another host - the iPhone - holds the pods: how long the Name row stays insensitive
  on the alias, waiting for the startup dump
  ([protocol/aap/rename.md](../protocol/aap/rename.md#open-questions)).

The checks of the pods themselves:
[protocol/aap/rename.md](../protocol/aap/rename.md#open-questions).
