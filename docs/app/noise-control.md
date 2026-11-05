# Noise control

How LinuxPods sets the noise control mode, and how it takes the pods' reports of it. The
packets and what each model answers are the pods':
[protocol/aap/noise-control.md](../protocol/aap/noise-control.md).

## Allowing Off

Why Off needs `0x34` first, and why that setting is not one to turn on lightly:
[protocol/aap/noise-control.md](../protocol/aap/noise-control.md#off-has-a-prerequisite-of-its-own).

`Coordinator::set_noise_control`, which carries out a `Command::NoiseControl`, sends
`0x34 01` only when Off is the mode asked for, immediately before the mode, rather than on
every connection. The setting persists on the pods and is believed to sync to the user's
Apple devices.

- **Back to back, with no delay,** which is enough. A link carries out its commands one at
  a time ([commands.md](commands.md#one-path)), so no other command goes out between the
  two.
- **A link that goes between the two** leaves Off allowed but not set. That is harmless:
  Off was asked for.

## Recorded, not confirmed

Nothing the pods answer confirms a change on every model:
[protocol/aap/noise-control.md](../protocol/aap/noise-control.md#command-acknowledgment).

**So the mode is recorded optimistically.** `set_noise_control` records it on the link
(`Link::noise_mode`) as soon as the packet is away, and broadcasts. The command is then
done, and its caller hears so ([commands.md](commands.md#what-the-caller-hears)). A later
`0x0D` report confirms the mode - unless it is a stale one, below. Waiting would leave the
window on the old mode meanwhile, or for good.

**The mode goes with the link.** It is kept apart from the reading every battery packet
replaces, and goes with the link like the ear state. A mode kept past the link showed the
last session's mode after a reconnect, though it may have been changed from a phone
meanwhile (1fa1632). The next link's startup dump reports it
([protocol/aap/noise-control.md](../protocol/aap/noise-control.md#initial-state-report)).

**A report is read only by its header, byte 4 and byte 6.** The `0x09` family carries a
dozen other sub-commands, all of them in the startup dump
([protocol/aap/settings.md](../protocol/aap/settings.md)).

**Levels `08` and `09` confirm nothing.** With Conversation Awareness on they frame every
mode change, and name no mode
([protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#levels-08-and-09-around-a-mode-change)).

## Stale reports

The stale report a Pro 3 sent after a switch to Off:
[protocol/aap/noise-control.md](../protocol/aap/noise-control.md#command-acknowledgment).

Taken as it came, the stale report put the old mode back on show for a second (#113). So a
report of the mode the last command replaced is held off (`aap::noise::ModeState`, on the
link):

| A report of | Within `aap::noise::STALE_REPORT_WINDOW` of the command, before the mode set is reported | Otherwise |
|---|---|---|
| The mode the last command replaced | Dropped | Taken |
| The mode set | Taken; ends the hold | Taken |
| Any other mode - changed elsewhere, or an earlier command's echo | Taken; ends the hold | Taken |

- **Dropped, not replayed** at the end of the window. On AirPods 4, Transparency and
  Adaptive each took effect and were followed by a report of the old mode, with no echo
  of the new one after it
  ([run 1](../protocol/aap/noise-control.md#run-1-initial-validation)): replayed, that
  report would show the wrong mode just the same. If a dropped report was right after all,
  the next report - an ear event's, say - corrects the mode.
- **Past the window, any report wins:** the mode may have been changed on another device.
- **Only the last command's replaced mode is held off,** and that is accepted. A report of
  a mode an earlier command set or replaced is taken, so quick successive changes can
  briefly replay older modes as their echoes arrive. The last command's echo comes last
  and settles it: the mode on show always ends as the one last set.

## Reports from elsewhere

The reports the pods send of a change made elsewhere:
[protocol/aap/noise-control.md](../protocol/aap/noise-control.md#unsolicited-mode-changes).

They are taken - but for a report of the mode a command here replaced, within the window
([Stale reports](#stale-reports)).

## The pods switch the mode themselves

The pods' own switch to Transparency while the user speaks:
[protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#the-pods-switch-the-mode-themselves).

The app takes these as it takes any report, so the window and the tray show Transparency
while the user talks. That is the mode the pods are in.

A mode set from the app during a stretch of speech is held as any command's is: a report
of Transparency within the window is taken for a stale one and dropped, and the next
report - the echo, or the switch back after the speech - settles what is shown. What the
pods do with a mode set during speech is not recorded; the fake pods go back to it after
the stretch.

## Hardware checks

- A mode set from the window during speech, with Conversation Awareness on: which mode
  the pods end in, and what the window shows.
- A mode changed on an iPhone while the pair is linked here: whether it shows.
