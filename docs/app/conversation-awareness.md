# Conversation Awareness

How LinuxPods records the Conversation Awareness setting, and what it does with the speech levels.

## The setting

What the pods report of the setting, and when: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#the-setting-0x09-sub-command-0x28).

So the app records the setting optimistically, as it does the noise mode: `Coordinator::set_conversation_awareness` records it once the packet is away. A report that does come is taken whenever it comes. A change made on a phone while the pair is linked here may never reach the app, and then the switch shows the old setting until the next connection's dump: accepted, until the [hardware checks](../protocol/aap/conversation-awareness.md#open-questions) say otherwise.

## In the app

- `aap::decode` reads the setting as `Message::ConversationAwareness` and a level as `Message::SpeechLevel`.
- The setting is kept on the link and shown in the snapshot (`PodState::conversation_awareness`); it goes with the link, as the noise mode does.
- `Command::ConversationAwareness` sends the packet to turn it on or off (taken by a Pro 3, 2026-11-02), through `Coordinator::command` like the noise mode. The Features group's switch sends it, and is sensitive while the device shown is on AAP and has reported the setting.
- The last level is kept on the link and in the snapshot (`PodState::speech_level`), and logged at debug. Every level also goes, as an event, to what lowers the volume: `Coordinator::speech_events` - see [volume.md](volume.md).
