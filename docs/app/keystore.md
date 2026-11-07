# Keystore

How Cymba asks each pair for its keys, which keys it takes, and where it keeps them.
The request and the key packet are the pods': [protocol/aap/keys.md](../protocol/aap/keys.md).
What the keys are used for: [identification.md](identification.md).

## Requested on every connection

The keys are requested on every AAP connection, not only when none is stored.

- **What would change them is not known** ([protocol/aap/keys.md](../protocol/aap/keys.md#key-persistence)):
  a Pro 2 (USB-C) forgotten and paired again with the same computer kept both.
- **A stale key fails without a word.** A stale IRK resolves none of the pair's new
  addresses, and a stale ENC_KEY fails the suffix check. Either way the pair drops off BLE
  altogether, as an advertisement nothing identifies is dropped.
- **Unchanged keys cost nothing:** they are not written again (below).

**The request is one packet,** which `Coordinator::open_aap` sends right after
`aap::Client::open` ([aap-links.md](aap-links.md#opening-a-link)). `open` leaves it out,
so a probe never receives keys it did not ask for, and prints them.

**No wait is bounded.** The link's read loop takes a key packet whenever it comes, for as
long as the link lasts; one that never comes is asked for again at the next connection.
The probes wait a window of their own: [development/probes.md](../development/probes.md#key_request).

## Which keys are taken

The read loop (`aap_read_loop`) hands each key packet to `aap::find_encryption_key` and
`aap::find_irk`, which each take a key of 16 bytes only. The length check keeps a
malformed key from replacing a working one.

| The packet | What is kept |
|---|---|
| A 16-byte ENC_KEY and a 16-byte IRK | Both |
| The stored ENC_KEY, no usable IRK | The stored IRK, with it |
| A new ENC_KEY, no usable IRK | The new ENC_KEY alone: the pair falls back to its key alone ([identification.md](identification.md#pairs-with-no-irk)) rather than vanishing |
| No usable ENC_KEY | Nothing: the packet is rejected |

The keys go into use at once, whether or not they reach the disk.

## Written only when changed

The keystore is written only when the keys in memory differ from those last saved
successfully (#38). So an unchanged key costs no write, and a save that failed is tried
again when the keys next arrive. Keys not saved before are logged as they are written:

```
Stored the keys of <MAC>
```

**Never left half done.** The save moves its owned lock into `spawn_blocking`, where no
abort reaches it. The read loop calling it is dropped wherever it is when its pair
detaches ([aap-links.md](aap-links.md#the-supervisor)). Aborted mid-save, the lock went
while the write ran on, and a second save raced it on the same temporary file, the older
keys landing last.

**The keystore lock is taken before `inner`, never under it,** as the readings lock is
([readings.md](readings.md#saving)).

## The file

`~/.local/share/cymba/keys.json` (the XDG data directory), mode 0600, written to a
temporary file and renamed over the old one. A file that does not load is set aside, not
overwritten.

**Format version 2** keeps the IRKs in an `irks` map beside `keys`, which holds the
ENC_KEYs:

- a version 1 file loads unchanged, with no IRK for any pair; each gets one at its next AAP
  connection, and the save that follows writes version 2;
- a version 1 reader still finds the ENC_KEYs of a version 2 file, rather than failing on it
  and setting the whole file aside.

**The IRK is kept as AAP sends it,** least significant byte first. `ble::rpa::resolves`
reverses it to resolve, and its test pins the order with the specification's sample
(Core spec Vol 3 Part H, Appendix D.7):
[protocol/ble/addresses.md](../protocol/ble/addresses.md#resolving-the-advertising-address).
A model that sent its IRK the other way round would have none of its addresses resolved,
and go unidentified. `examples/irk_probe.rs` reports an address that resolves only with the
IRK unreversed.

## Hardware checks

None open. Keys taken and kept on a Pro 3 and a Pro 2 (USB-C) (#71, #134).
