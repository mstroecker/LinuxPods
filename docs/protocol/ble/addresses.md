# Advertising addresses

The addresses AirPods advertise from: resolvable private addresses, one per pod, that
change every few minutes, and that only the pair's IRK ties back to the pair. Checked
here on two models only, AirPods Pro 3 and AirPods Pro 2 (USB-C) (see [Runs](#runs));
other models are untested.

Resolved by `ble::rpa`; what LinuxPods does with it:
[app/identification.md](../../app/identification.md).

Addresses are written as usual, most significant byte first: in `AA:BB:CC:DD:EE:FF`,
`AA` is the first byte.

## MAC Address Randomization and Device Identification

### Which address is used where

AirPods use Bluetooth LE privacy in their advertisements:

| Where | Address |
|---|---|
| BLE advertisements | A resolvable private address, one per pod, changing from time to time (Pro 3 and Pro 2 (USB-C), #71, #134) |
| AAP connections | The real, permanent MAC (every AAP run here, as in #71) |

The point of the private address is that only a holder of the pair's IRK can tell that
an address is the pair's (Core spec Vol 3 Part C 10.7). Anyone else cannot track the
pods by their advertisements.

### Rotation

- **One address per pod.** Both pods advertise, each from its own address (Pro 3,
  2026-10-25, #71).
- **A new address every 4 to 8 minutes,** connected or not (Pro 3 and Pro 2 (USB-C),
  2026-10-28, #71).
- **Each pod rotates on a clock of its own** (Pro 3, 2026-10-28, #71). The Pro 2
  (USB-C)'s pods both changed address at once, 25 s after `bluetoothctl block`
  disconnected them (2026-10-28, #71).
- **A new address across a closed lid:** both pods came back with new addresses after
  the lid had been closed, on both models (Pro 3, 2026-10-25; Pro 3 and Pro 2 (USB-C),
  2026-10-28; #71).
- **A silence at each change:** between a pod's last advertisement from the old address
  and its first from the new one, nothing was heard from it for one to three minutes
  (Pro 3 and Pro 2 (USB-C), 2026-10-28, #71).

So a pair's advertisements cannot be matched to its stored keys by address. The IRK
does it.

### Resolving the Advertising Address

The IRK (key type `0x01`, sent over AAP: [aap/keys.md](../aap/keys.md)) tells which pair
an advertisement comes from. A resolvable private address, as written, is (Core spec
Vol 6 Part B 1.3.2.2):

| Bytes | Field |
|---|---|
| 0-2 | `prand`; its top two bits are `01` |
| 3-5 | `hash` |

The address resolves with an IRK when the low 24 bits of `AES-128(IRK, 0^104 || prand)`
equal `hash`: the random address hash `ah()` (Core spec Vol 3 Part H 2.2.2).

**Byte order.**

- `ah()` takes the IRK most significant byte first (Core spec Vol 3 Part H 2.2.2),
  and the address as written: in `AA:BB:CC:DD:EE:FF`, `prand` is `AA BB CC` and the
  hash `DD EE FF` (Core spec Vol 6 Part B 1.3.2.2).
- The key packet carries the IRK **byte-reversed**: least significant byte first, as
  SMP carries keys (#134).
- So it has to be reversed before it is used. Used as it comes, it resolves nothing
  (Pro 3 and Pro 2 (USB-C), 2026-10-28, #134).
- In the specification's sample data (Core spec Vol 3 Part H Appendix D.7), the IRK
  `ec0234a3 57c8ad05 341010a6 0a397d9b`, most significant byte first, resolves the
  address `70:81:94:0D:FB:AA` (`prand` `708194`, hash `0dfbaa`).

**On hardware,** every advertising address of the two models tested is a resolvable
private address, and resolves with the IRK the pair sends over AAP: 12 of 12 addresses
across the two models, connected, disconnected and across a lid close. Every
advertisement from them also decrypted with the pair's ENC_KEY, and the two checks never
disagreed (Pro 3 and Pro 2 (USB-C), 2026-10-28, #134; [Runs](#runs)).

### False matches

- **A wrong IRK** resolves a given address by chance one time in 2^24: the hash has 24
  bits (Core spec Vol 6 Part B 1.3.2.2). An address lasts minutes, so that chance comes
  once per address and IRK, not once per advertisement.
- **A wrong ENC_KEY** passes the suffix check one time in 2^23, once per advertisement
  and key: [proximity-pairing.md](proximity-pairing.md#decryption-validation).
- **A recorded advertisement replayed from another address** does not resolve with the
  pair's IRK.

Where LinuxPods keeps the IRK and reverses it:
[app/keystore.md](../../app/keystore.md#the-file). Which advertisements it
counts: [app/identification.md](../../app/identification.md#what-counts).

### Multi-Device Workflow

Identifying an advertisement among several pairs, with each pair's keys stored under
its real MAC. The addresses are placeholders: `PP:PP:PP:HH:HH:HH` is a resolvable
private address with `prand` `PPPPPP` and hash `HHHHHH`.

```
Device A (real MAC 11:22:33:44:55:66)
  └─> Store keys: 11:22:33:44:55:66 -> IRK A, ENC_KEY A

Device B (real MAC AA:BB:CC:DD:EE:FF)
  └─> Store keys: AA:BB:CC:DD:EE:FF -> IRK B, ENC_KEY B

BLE advertisement received (address PP:PP:PP:HH:HH:HH)
  ├─> Resolve with IRK A: ah(IRK A, PPPPPP) != HHHHHH -> ❌ not this device
  └─> Resolve with IRK B: ah(IRK B, PPPPPP) == HHHHHH -> ✅ device B's address
        └─> Decrypt with ENC_KEY B only
              └─> plaintext bytes 7-9 = DD EE FF (or 00 00 00) -> ✅ match
                    └─> Device identified as AA:BB:CC:DD:EE:FF
```

The decryption and the suffix check:
[proximity-pairing.md](proximity-pairing.md#decryption-validation). How LinuxPods keys
what it identifies, and drops the rest:
[app/identification.md](../../app/identification.md#keyed-by-the-real-mac).

## Runs

The #134 IRK probe, 2026-10-28, on a Pro 3 and a Pro 2 (USB-C), one adapter (#134,
#71):

```bash
cargo run --example irk_probe -- <MAC_ADDRESS> [<MAC_ADDRESS>...]
```

It prints numbered addresses, the cleartext bytes 3-8, and whether each address
resolves and decrypts; no keys and no addresses.

| | Pro 3 | Pro 2 (USB-C) |
|---|---|---|
| Phases | Connected (on the table, worn, in the case), blocked with no host for 13 min, lid closed, lid open | Connected (in the case, worn), blocked, lid closed, lid open |
| Addresses of the pair | 6, all resolvable private addresses | 6, all resolvable private addresses |
| Resolved with the pair's IRK, reversed | 6 of 6 | 6 of 6 |
| Advertisements the ENC_KEY decrypted | 134 of 134 | 96 of 96 |
| Advertisements where IRK and ENC_KEY disagree | None | None |

- **Rotation:** on the Pro 3, blocked, lid open, untouched, one pod's address lasted
  from 0 to 247 s, and its next first came at 318 s; the other pod's lasted from 37 to
  514 s, and its next first came at 703 s. The Pro 2, connected to this host and
  untouched, kept its address from 0 to 507 s, and the next came at 676 s.
- **Silences:** 70 s and 190 s on the Pro 3, 170 s on the Pro 2. The probe hears only
  what BlueZ reports.

## Open questions

- Does a disconnect trigger rotation? The Pro 2 (USB-C)'s pods changed address 25 s
  after `bluetoothctl block`; the Pro 3's did not (2026-10-28, #71). One sample.

- Models other than the Pro 3 and the Pro 2 (USB-C): whether their addresses resolve,
  and whether their key packet carries the IRK byte-reversed too.
  `examples/irk_probe.rs` checks a new model: it reports an address that resolves only
  with the IRK unreversed.
