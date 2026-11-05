# Advertising addresses

The resolvable private addresses AirPods advertise from, how they rotate, and how a pair's IRK resolves them.

## MAC Address Randomization and Device Identification

### BLE Privacy Feature

AirPods use **Bluetooth LE Privacy** (MAC address randomization) in their BLE advertisements:

- **BLE Advertisements**: Come from resolvable private addresses (RPAs) that change from
  time to time
- **AAP Connections**: Use the real (permanent) MAC address
- **Privacy Goal**: Prevent tracking of AirPods via BLE advertisements, by anyone who does
  not hold the pair's IRK

### Device Identification Challenge

When monitoring multiple AirPods devices:
1. Each device has a real MAC address (from AAP connection)
2. Each device's BLE advertisements use randomized MACs - one per pod, as both pods
   advertise
3. The randomized MACs change on a timer and across a closed lid. In the #71 captures
   an address lasted about 4 to 8 minutes, connected or not, each pod rotating on a
   clock of its own; and both pods came back with new addresses after the lid had been
   closed, on both models. Between a pod's last advertisement from the old address and
   its first from the new one, nothing was heard from it for one to three minutes
4. **Problem**: Cannot directly match BLE advertisements to stored keys by address

### Resolving the Advertising Address

The **IRK** (type 0x01) tells which pair an advertisement comes from. The address is
a resolvable private address: its top two bits are `01`, the first three bytes are
`prand` and the last three a hash, as the address is written. Reverse the IRK, then
check that the low 24 bits of `AES-128(IRK, 0^104 || prand)` equal the hash. See
[Device Identification](../../app/identification.md#solution-identification-by-irk-then-enc_key).

Every advertising address of the two models tested is a **resolvable private address**,
and resolves with the IRK the pair sends over AAP: 12 of 12 addresses across the two
models, connected, disconnected and across a lid close (#134, `examples/irk_probe.rs`).
Every advertisement from them also decrypted with the pair's ENC_KEY, and the two checks
never disagreed.

**Byte order.** `ah()` takes the IRK most significant byte first, and the address
written as usual, most significant byte first: in `AA:BB:CC:DD:EE:FF`, `prand` is
`AA BB CC` and the hash `DD EE FF`. The key packet carries the IRK **byte-reversed**,
least significant byte first as SMP carries keys: it has to be reversed before it is
used as the key. Resolved as it comes, it resolves nothing. In
the specification's sample data (Appendix D.7), the IRK `ec0234a3 57c8ad05 341010a6
0a397d9b`, most significant byte first, resolves the address `70:81:94:0D:FB:AA`
(`prand` `708194`, hash `0dfbaa`).

Where LinuxPods keeps the IRK and reverses it: [app/keystore.md](../../app/keystore.md#the-irks-byte-order).

### Multi-Device Workflow

```
Device A (Real MAC: 11:22:33:44:55:66)
  └─> Store keys: 11:22:33:44:55:66 -> IRK A, ENC_KEY A

Device B (Real MAC: AA:BB:CC:DD:EE:FF)
  └─> Store keys: AA:BB:CC:DD:EE:FF -> IRK B, ENC_KEY B

BLE Advertisement received (Random MAC: 4A:1C:93:7E:20:D5)
  ├─> Resolve with IRK A: ah(IRK A, 4a1c93) != 7e20d5 -> ❌ not this device
  └─> Resolve with IRK B: ah(IRK B, 4a1c93) == 7e20d5 -> ✅ device B's address
        └─> Decrypt with ENC_KEY B only
              └─> decrypted[7..10] = DD EE FF (or 00 00 00) -> ✅ match
                    └─> Device identified as AA:BB:CC:DD:EE:FF
```

How LinuxPods keys what it identifies, and drops the rest: [app/identification.md](../../app/identification.md#multi-device-workflow).
