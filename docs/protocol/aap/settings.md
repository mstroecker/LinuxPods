# AAP settings

The `0x09` family of settings sub-commands, as the pods report them in the startup dump.

## Settings Command Family (0x09)

The `0x09` command byte is a family of settings sub-commands, not exclusive to noise control. Observed sub-commands during connection startup, with the data where it was recorded: AirPods 4's from the test runs in [noise-control.md](noise-control.md#test-results), and AirPods Pro 3's from #71, whose dump held 21 settings and neither `0x1A` nor `0x34`:

| Sub-cmd (byte 6) | Notes (AirPods 4) | AirPods Pro 3 (#71) |
|-------------------|-------|-------------|
| `0x0D` | **Noise control mode** | the mode |
| `0x17` | Unknown | seen |
| `0x1A` | Listening mode configs - bitmask of the modes in the press-and-hold cycle | not in the dump |
| `0x34` | **Allow Off as a listening mode** - the prerequisite for `0x0D 01` | not in the dump |
| `0x18` | Unknown | seen |
| `0x1B` | Unknown | seen |
| `0x1F` | Unknown (data: `50 50`) | `50 50` |
| `0x23` | Unknown | seen |
| `0x24` | Unknown (data: `03`) | `00 03` |
| `0x25` | Unknown (data: `01`) | `01` |
| `0x26` | Unknown (data: `01`) | `02` |
| `0x28` | **Conversation Awareness** - `01` on, `02` off; see [conversation-awareness.md](conversation-awareness.md) (data: `01`) | `02` (`01` in the #150 run) |
| `0x29` | Unknown | seen |
| `0x2C` | Unknown | seen |
| `0x2E` | Unknown (data: `32`) | `32` |
| `0x2F` | Unknown | seen |
| `0x30` | Unknown | seen |
| `0x33` | Unknown | seen |
| `0x35` | Unknown | seen |
| `0x37` | Unknown | seen |
| `0x38` | Unknown | `00`, then `52` in the second capture |
| `0x3B` | Unknown | seen |
| `0x3E` | Unknown | seen |

When parsing `0x09` packets, always check byte 6 to identify the specific sub-command. Do not assume all `0x09` packets are noise control.
