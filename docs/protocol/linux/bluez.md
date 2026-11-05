# BlueZ

What bluetoothd does that a client has to know: discovery, the A2DP transport's volume, and a device's name and alias.

## Discovery

- **Discovery:** bluetoothd removes the pods' devices when the last client stops
  discovering, as it does every temporary device that is not connectable and not
  connected (`discovery_cleanup` in BlueZ's `src/adapter.c`); the pods'
  advertisements are not connectable. So with nobody else discovering, a discovery
  start hears each pod at once, in the `InterfacesAdded` that announces its device:
  the battery showed 36 ms after the app started discovering, on 2026-11-02. While
  another client discovers - GNOME Settings' Bluetooth panel, say - the devices stay,
  and a pod is heard only once its payload or address changes: after 2.1 s and 4.1 s
  that day (#153)

Why few advertisements reach the app while a pair is connected: [app/in-ear.md](../../app/in-ear.md#why-aap-and-not-the-advertisement).

## MediaTransport1.Volume

`org.bluez.MediaTransport1.Volume` (`q`, 0-127, writable, signaled), the AVRCP absolute
volume of the pair's A2DP transport. It goes to the pods themselves. BlueZ's
documentation (`org.bluez.MediaTransport(5)`) says `Volume` is writable only by the
client that acquired the transport; bluetoothd checks no owner for it
(`profiles/audio/transport.c`), and `busctl` was not the owner. The run that set it
with `busctl`: [desktop.md](desktop.md#the-volume).

BlueZ makes a transport for each A2DP setup, at a path of its own under the device -
`<device>/sep1/fd<n>`, `n` the first number free from the count of all bluetoothd's
transports (`media_transport_create` in BlueZ 5.87's `transport.c`).

A `Set` of `Volume` changes nothing at once, nor necessarily to the volume set. In BlueZ 5.87's source, a host-as-source transport's setter (`transport_a2dp_src_set_volume` in `transport.c`) does nothing for the volume the transport holds, and otherwise sends the volume to the pods over AVRCP (`avrcp_set_volume`) and changes nothing; the property changes once the pods answer (`avrcp_handle_set_volume` in `profiles/audio/avrcp.c`), to the volume they report, which may be rounded. That is a reading of the source: how long the pods take, and whether they round, is untried here; the Pro 3 read 20 back as set with `busctl`, some time after the set.

bluetoothd keeps the volume on the device and starts its next transport at it (`btd_device_set_volume` in `transport.c`).

What the app does with each of these: [app/volume.md](../../app/volume.md).

## Device1.Name and Device1.Alias

The pods' own name, which BlueZ reads as `Device1.Name`: [protocol/aap/rename.md](../aap/rename.md#two-names).

- **BlueZ's alias**: `Device1.Alias`, this computer's label for the pair, which GNOME Settings' rename sets and which never reaches the pods (#40, PR #62). Until one is set, BlueZ gives the name as the alias, so the alias follows the pods' name. Once set, it stays whatever the pods are called. Setting it to an empty string clears it; typing the old name back in GNOME does not.

## Open questions

- Do the pods answer a set with another volume than the one set, and by how much? No run of its own: the app's debug log (`RUST_LOG=linuxpods::volume=debug`) of a stretch of speech shows each volume set beside what the transport read then, and `examples/conversation_awareness.rs` with `--duck` what it read back after each set.
