# AAP links

How LinuxPods opens the AAP session to a pair: the connect and the handshake.

## Step 1: Connect to AirPods

Why the connect is waited for: [protocol/linux/l2cap.md](../protocol/linux/l2cap.md#connecting).

`Client::connect` does the waiting.

## Step 2: Send Handshake

Why nothing goes out before the answer: [protocol/aap/session.md](../protocol/aap/session.md#step-2-send-handshake).

`Client::handshake` returns once the answer is in, and fails if none comes within 5 s.

## The features packet

What the `0x4D` packet is for: [protocol/aap/session.md](../protocol/aap/session.md#prerequisites-the-features-packet).

Every link sends the `0x4D` packet as it comes up (`aap::noise::ENABLE_FEATURES`).
