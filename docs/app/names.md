# Names

How LinuxPods names a pair: the pods' own name beside BlueZ's alias, the Name row, and the rename dialog.

## Two names

The pods' name and BlueZ's alias: [protocol/aap/rename.md](../protocol/aap/rename.md#two-names), [protocol/linux/bluez.md](../protocol/linux/bluez.md#device1name-and-device1alias).

The app never touches the alias, neither setting nor clearing it. Set on every rename, it would pin the name, so that a later rename on the pods - from an iPhone, say - never showed here; cleared, it would throw away a name the user gave in GNOME. With no alias set, the new name shows in GNOME at the next reconnect on its own. Whether one is set the app reads as `Alias != Name`, from the names attachment reads with `GetManagedObjects` on every connection BlueZ confirms (`bluez::DeviceNames`). An alias set in GNOME while the pair stays connected is seen at its next connection event.

The Name row shows the pods' name where the link has reported it, else BlueZ's alias, and opens the dialog only once the link has reported the pods' name: started from the alias, Enter would rename the pods to it. The dialog's Rename stays insensitive for the name the pods have.

Elsewhere - the switcher in the header bar, a low battery notification - the app names a pair as the rest of the desktop will once BlueZ has caught up (`Snapshot::display_name`): an alias set in BlueZ, else the pods' name where the link has it, else BlueZ's alias, then the model. Named by the alias alone, the switcher would keep the old name after a rename until the pair connected again: BlueZ's `Name` and `Alias` stayed old until then in the [Pro 3 run](../protocol/aap/rename.md#hardware-run-pro-3-2026-11-02) (#160). An alias typed back to the pods' old name is the exception: it reads as unset until the pair connects again, so the switcher shows the pods' new name until then, and the alias after; the dialog's body is wrong then too, saying Settings shows the new name once the AirPods connect again, when Settings keeps the alias. Off AAP the pods' name is not known, so a pair renamed and gone shows the old name until BlueZ reads the new one.

## Rename: `0x1A`

The packet, and what the pods do with a name: [protocol/aap/rename.md](../protocol/aap/rename.md#rename-0x1a).

- Fire and forget, like the noise mode: the app records the name on the link once the packet is away (`Link::name`).
- The app refuses an empty name, a NUL and anything over 127 bytes before anything is sent: in the dialog, in `Coordinator::command` before the command is queued, and in `aap::name::rename_packet`.

## Hardware run

What the run showed of BlueZ's `Name` and of Apple devices: [protocol/aap/rename.md](../protocol/aap/rename.md#hardware-run-pro-3-2026-11-02).

- **BlueZ's `Name` catches up at the next reconnect**: the dialog says so, and the app forces no reconnect.
- **Apple devices keep their own name**: the dialog says so too.

## Name run

What the pods keep of a long name: [protocol/aap/rename.md](../protocol/aap/rename.md#name-run-pro-3-2026-11-02).

Any name up to 127 bytes is allowed, whatever its characters; over 127 bytes is refused, never sent, and never cut by bytes. In the dialog the entry's `max-length` of 127 stops a text at 127 characters, and cuts a longer paste to them, exact for ASCII; a shorter name with characters beyond ASCII can still pass 127 bytes, and gets the error style and a line saying it is too long, with Rename insensitive.

How the fake pods take a rename: [development/fake-world.md](../development/fake-world.md#rename).

## Hardware checks

The app's rename has run against the fake world only.

- Rename from the window on the Pro 3: the name in the next session, and after a reconnect in GNOME.
- With an alias set in GNOME Settings: the dialog's other body, and the alias left as it was.
- A name over 127 bytes refused in the dialog; a failed send's toast.
- After a rename, the switcher shows the new name at once, and one reconnect shows it in GNOME, without a second reconnect.
- With an alias set in GNOME Settings, the switcher keeps the alias after a rename.
- The dialog on real GNOME: the focus, the cursor at the end, Enter, a long paste cut to 127 characters.
- If `0x1D` waits for `0x0C` to name this host ([protocol/aap/rename.md](../protocol/aap/rename.md#open-questions)): then the Name row stays insensitive, on the alias, for as long as the other host is active.

The checks of the pods themselves: [protocol/aap/rename.md](../protocol/aap/rename.md#open-questions).
