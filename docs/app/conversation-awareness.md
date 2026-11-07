# Conversation Awareness

How Cymba records the Conversation Awareness setting, and what it does with the speech
levels. The setting and the levels are the pods':
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md).
What the levels lower: [volume.md](volume.md).

## The setting

What the pods report of the setting, and when:
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#the-setting-0x09-sub-command-0x28).

**Recorded optimistically, like the noise mode.** That the pods do not report the setting
again when it changes is LibrePods' word, not yet checked here (#150). So
`Command::ConversationAwareness` records it on the link (`Link::conversation_awareness`)
once the packet is away. A report that does come is taken whenever it comes.

The cost, accepted until the [open questions](../protocol/aap/conversation-awareness.md#open-questions)
say otherwise: a change made on a phone while the pair is linked here may never reach the
app, and the switch shows the old setting until the next connection's dump.

- **Read only with byte 4 and byte 6 checked,** as the noise mode is: `0x09` sub-command
  `0x28`, decoded as `Message::ConversationAwareness`.
- **Sent through `Coordinator::command`,** like the noise mode
  ([commands.md](commands.md)). The same packet sets it (a Pro 3, 2026-11-02).
- **Goes with the link,** and shows in the snapshot (`PodState::conversation_awareness`).
- **The switch** in the window's Features group is sensitive while the pair shown is on AAP
  and has reported the setting: a switch cannot show unknown ([ui.md](ui.md#controls-that-send-commands)).

## The speech levels

What the levels mean: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#speech-levels-0x4b).

- **Read only with bytes 5-8 checked.** `aap::conversation::is_level_packet` takes a `0x4B`
  only with `00 02 00 01` there, as in every one seen; one shaped otherwise is no level it
  knows. `aap::decode` reads it as `Message::SpeechLevel`.
- **A level is a target, not a step to count,** as the pods skip levels
  (`aap::conversation::Speech`).
- **`08` and `09` name no mode and acknowledge nothing:** they also frame every mode change
  ([noise-control.md](noise-control.md#recorded-not-confirmed)).
- **The link's last level** is in the snapshot (`PodState::speech_level`), and logged at
  `debug`.
- **Every level goes to the volume as an event** (`Coordinator::speech_events`,
  [state.md](state.md#events)), in the order the pods sent it.

While the user speaks, the pods switch to Transparency and back themselves:
[noise-control.md](noise-control.md#the-pods-switch-the-mode-themselves).

## Hardware checks

Checked on a Pro 3 on 2026-11-02: the window's switch changes the setting. The checks of
the setting changed on a phone are the pods':
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#open-questions).
