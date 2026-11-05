# State

How the coordinator keeps what the advertisements report: here, the lid.

## The lid

Which pod's advertisements read the lid: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-6-lid-state).

The parser reads the lid from byte 6 bit 3 only while status bit 6 or bit 2 puts a pod
in the case, and reports no lid otherwise. It does not read byte 6's high nibble.

Taken one by one, the lid would blink between open and unknown several times a
second. So an advertisement whose sender is out of the case but whose status bit 4
says a pod is in it keeps the lid the coordinator last had for that device; one with
bit 4 clear too means no pod is in the case, and no lid. The parser reports bit 4 as
`one_in_case` and stays stateless; `Coordinator::handle_advertisement` does the
keeping.

When the pods stop advertising after the lid closes: [protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-6-lid-state).

The coordinator's cached reading keeps a closed lid on show until the pods advertise
again, for up to `BLE_CACHE_TTL` (a day), as long as the app runs. What a relaunch
keeps of it: [readings.md](readings.md#the-lid).
