# Firmware

How LinuxPods reads the firmware version from the device information packet, keeps it, and shows it.

## Each field on its own

The packet's fields: [protocol/aap/device-info.md](../protocol/aap/device-info.md#fields).

Each field is read on its own (`aap::info::DeviceInfo`): a name that is not UTF-8, as a name the pods cut inside a character would be, is not taken, and the version is all the same; a version that does not read leaves the name. The packet as a whole never fails to decode once its preamble is right.

## The firmware version: field 10

How field 10 packs the version: [protocol/aap/device-info.md](../protocol/aap/device-info.md#the-firmware-version-field-10).

The field must be decimal digits alone, within a u32, or the version is not read. It is found by its place alone, as the eleventh string, which held on the two models seen; on a model whose `0x1D` has another count of strings, whatever stands eleventh would be read, if it were all digits - unlikely for a serial number, as Apple's mix letters and digits. The app logs field 10's value beside its reading of it as the version arrives - `Firmware reported by <MAC>: 9A348 (Marketing Version 9442752)` - so a wrong reading shows.

## What the app does with it

- **Kept for the run, connected or not.** The version comes with each AAP connection's `0x1D` and is kept per pair after the link goes (`Inner::firmware_versions`, in the snapshot as `Snapshot::firmware_versions`), like the BLE cache but never expiring, unlike the rest of AAP, which goes with the link; the next connection's replaces it. An update installed through an iPhone shows at the pair's next AAP connection. A `0x1D` read on a link that has since been replaced is dropped, as every packet of the old link is.
- **The Firmware row** in the Info group, below Name, shows the version as the iPhone does, dimmed and selectable, so it can be copied into a bug report. It is no command and opens nothing. Before the pair's first AAP connection in a run there is no version: the row shows "Unknown" and is insensitive, so the group keeps its shape.

How the fake pods send the packet: [development/fake-world.md](../development/fake-world.md#device-information).

## Hardware checks

The app's Firmware row has run against the fake world only.

- On the Pro 3: the Firmware row shows `9A348`, matching the iPhone's Version; it stays after the pair disconnects; it shows "Unknown", insensitive, before the pair's first connection in a run.

The checks of the packet itself: [protocol/aap/device-info.md](../protocol/aap/device-info.md#open-questions).
