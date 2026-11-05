# Logging

What LinuxPods logs of the packets it reads, and what it never logs: here, the AAP packet lines and the device information packet.

## AAP packets

The `AAP packet (N bytes):` debug line shows the first 8 bytes of a packet; at `trace`, the whole packet follows (key packets redacted).

## Device information

The packet's fields, serial numbers among them: [protocol/aap/device-info.md](../protocol/aap/device-info.md#fields).

A `0x1D` without `02` at byte 6, or without `04 00` at bytes 9-10, is not read at all (`aap::info::is_info_packet`), as a `0x09` report is not read for a mode unless byte 6 names the setting; it is redacted all the same, by its opcode.

Only fields 0 and 10 leave the parser. The serial numbers are never parsed out, logged, kept or shown: `aap::keys::redacted_hex` cuts any `0x1D` to its 11-byte preamble wherever a packet is logged - the read loop's debug and trace lines, the probes - as it cuts a key packet to its preamble. A field that does not read carries none of its bytes in its error either, as a field out of place could be a serial number.
