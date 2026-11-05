# Keystore

How LinuxPods asks each pair for its keys, and where it keeps them.

## Key Persistence

What the pods do with their keys: [protocol/aap/keys.md](../protocol/aap/keys.md#key-persistence).

- Request them on every connection anyway: should they change, a stale IRK resolves
  none of the pair's new addresses and a stale key fails suffix validation, both
  without a word. Unchanged keys cost nothing: they are not written again
- Store securely (keyring, encrypted config file). LinuxPods keeps them in
  `~/.local/share/linuxpods/keys.json`, mode 0600: the ENC_KEY and, since format
  version 2, the IRK, as AAP sends it

## The IRK's byte order

Why the IRK is reversed before use, and the specification's sample: [protocol/ble/addresses.md](../protocol/ble/addresses.md#resolving-the-advertising-address).

`ble::rpa::resolves` resolves an address with the IRK as AAP sends it. The keystore
keeps the IRK as AAP sends it, and `ble::rpa::resolves` reverses it. Its test pins the
order with the specification's sample data (Appendix D.7) - reversed, as AAP would send
it, is what `resolves` takes.

A model that sent its IRK the other way round would have none of its addresses
resolved, and so go unidentified. `examples/irk_probe.rs` reports an address that
resolves only with the IRK unreversed.

## Reference Implementation

The app itself asks for the keys on every AAP connection: `Coordinator::open_aap`
sends the request once the session is up, and the link's read loop hands each key
packet to `aap::find_encryption_key` and `aap::find_irk`, which each take a 16-byte
key. A packet without a 16-byte ENC_KEY is rejected; one without a 16-byte IRK keeps
the IRK stored, as long as the ENC_KEY is the one stored with it. The keys go into
use at once, and are written to the keystore whenever the keys in memory differ from
those last saved successfully - so a save that failed is tried again when the keys
next arrive. If a pair's keys have changed, the new ones therefore arrive with the
next connection, and a request that went unanswered is sent again by reconnecting. Run
the app with the AirPods connected; keys it had not saved yet are logged as they are
written:
```
Stored the keys of <MAC>
```

The probes that request the keys outside the app: [development/probes.md](../development/probes.md#reference-implementation).
