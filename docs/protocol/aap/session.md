# AAP session

The L2CAP channel AAP runs on, the handshake, the features packet, and what the pods send as a session starts.

## Connection Details

### Protocol
- **Transport**: L2CAP (Logical Link Control and Adaptation Protocol)
- **PSM**: 0x1001 (4097 decimal)
- **Connection Type**: BR/EDR (Classic Bluetooth)
- **Socket Type**: SOCK_SEQPACKET

### Prerequisites
- AirPods must be paired with the system via Bluetooth
- AirPods must be powered on and in range
- Bluetooth adapter with L2CAP support

## Step 1: Connect to AirPods

Establish an L2CAP connection to the AirPods on PSM 0x1001.

What the kernel's connect does before the channel is up: [protocol/linux/l2cap.md](../linux/l2cap.md).

## Step 2: Send Handshake

Send a 16-byte handshake packet to initialize the connection:

```
Hex: 00 00 04 00 01 00 02 00 00 00 00 00 00 00 00 00
```

**Expected response:** a packet starting `01 00 04 00` (18 bytes on Pro 3).

⚠️ **Wait for it before sending anything else.** The AirPods drop what they are sent
before they have answered. A pair that had only just connected answered 1.2 s after
the handshake, and requests sent after a fixed 500 ms pause were never answered - no
battery, no keys.

How the app waits for the answer: [app/aap-links.md](../../app/aap-links.md#step-2-send-handshake).

The key request comes next: [keys.md](keys.md#step-3-send-key-request).

## Prerequisites: the features packet

The `EnableSpecialFeatures` packet must be sent before noise control commands. This is already part of the standard AAP connection sequence:

```
Feature Enable: 04 00 04 00 4D 00 FF 00 00 00 00 00 00 00
```

Without this packet, Adaptive mode may not be available, and, LibrePods says, Conversation Awareness works only while nothing plays ([conversation-awareness.md](conversation-awareness.md)).

When the app sends it: [app/aap-links.md](../../app/aap-links.md#the-features-packet).

## Initial State Report

On AirPods Pro 3 the dump does not come with the battery: it waits until a `0x0C` packet names this host as the one in use, which came 1.5-1.7 s after the notification request in the #71 captures. Until then the mode is unknown.

The noise control mode in the dump: [noise-control.md](noise-control.md#initial-state-report).

## Multiple Packets

The AirPods may send several packets after the key request:
- Some packets are status updates, or the unsolicited list of known hosts (`04 00 04 00 2B 00`),
  which comes with the handshake's answer. It is not an answer to the features packet, which
  nothing answers (#71)
- Only packets with the `04 00 04 00` header and `data[4] == 0x31` contain keys
- **Loop and check each packet** until keys are found
