# Probes

The examples in `examples/` that request keys and decrypt advertisements, what each prints, and what to try when one fails.

## Reference Implementation

How the app itself requests and stores the keys: [app/keystore.md](../app/keystore.md#which-keys-are-taken).

To debug a request that fails - on a new model, say - the `key_request` example
opens the same session, sends the request, and lists every packet that arrives,
laying a key packet out key by key:
```bash
cargo run --example key_request -- <MAC_ADDRESS> [SECONDS]
```

It prints each packet's opcode, length and kind; for a key packet, its header, its
total length and each key's type and length, and whether the app would take its
ENC_KEY (the 16-byte rule of `find_encryption_key`). Key bytes are never printed,
so the output can go into an issue, and nothing is stored: a debugging run must not
replace a key that works. If no key packet arrives within the window, 5 s by
default, it says so and lists what did arrive.

To check that a new model's advertising addresses resolve with its IRK, in the
order the app uses, `irk_probe` asks each pair named for its keys, keeps them in
memory, and scans, printing per address only whether it resolves and decrypts:
```bash
cargo run --example irk_probe -- <MAC_ADDRESS> [<MAC_ADDRESS>...]
```
It prints no keys and no addresses, and stores nothing.

## Testing

Use the retrieved ENC_KEY to test decryption ([protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#bytes-9-24-encrypted-battery-data)):

```bash
# Test with a known BLE payload
cargo run --example decrypt_probe

# Live scanning with decryption
RUST_LOG=linuxpods=debug cargo run
```

If decryption is working correctly, you should see:
- Byte 1: Left/Right pod battery (0-100%)
- Byte 2: Right/Left pod battery (0-100%)
- Bit 7 of each byte indicates charging status

## Troubleshooting

### "Connection refused"
- Ensure AirPods are paired via Bluetooth settings
- Check AirPods are powered on and in range

### "No key packet received"
- Wait longer: the key packet may come seconds after the request
- Check PSM is correct (0x1001)
- Verify nothing is sent before the handshake is answered (`01 00 04 00 …`): a key
  request sent sooner is dropped, with no error

### "Decryption produces garbage"
- Wrong key type (use ENC_KEY, not IRK)
- Wrong key extracted (check parsing logic)
- BLE payload from a different AirPods device - expected when trying every stored
  key; resolve the address with each IRK first, and the MAC suffix at bytes 7-9
  confirms the match
- Validating with magic bytes rather than the MAC suffix. The old marker (byte 0
  upper nibble `0x0`, byte 4 `0x2D`) rejects correct decryptions on current models

### "Keys change after re-pairing"
- Not a given: a pair forgotten and paired again kept its keys (see
  [Key Persistence](../protocol/aap/keys.md#key-persistence))
- If the keys do change, reconnect over AAP: the app asks again on every connection,
  and an old IRK resolves none of the pair's new addresses
