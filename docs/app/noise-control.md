# Noise control

How LinuxPods sets the noise control mode, and how it takes the pods' reports of it.

## Off has a prerequisite of its own

Why Off needs `0x34` first, and why that setting is not one to turn on lightly: [protocol/aap/noise-control.md](../protocol/aap/noise-control.md#off-has-a-prerequisite-of-its-own).

So `Coordinator::set_noise_control`, which carries out a `Command::NoiseControl`,
sends it only when Off is the mode being asked for, immediately before the mode
command, rather than enabling it on every connection. The two go out back to
back with no delay, which is enough; and as a link carries out its commands one
at a time, no other command goes out between them.

## Command Acknowledgment

What answers a mode change on each model: [protocol/aap/noise-control.md](../protocol/aap/noise-control.md#command-acknowledgment).

**Integration note:** Treat noise control commands as fire-and-forget. Update local state optimistically after sending — do not wait for a `0x0D` confirmation packet. `Coordinator::set_noise_control` does exactly that: it records the mode as soon as the packet is away and broadcasts, and the command is done - its caller, waiting on `Coordinator::command`, hears so then. A later `0x0D` report merely confirms the mode - unless it is a stale one, below.

## Stale reports

The stale report a Pro 3 sent after a switch to Off: [protocol/aap/noise-control.md](../protocol/aap/noise-control.md#command-acknowledgment).

**Stale reports.** Taken as it came, the stale `0D 02` put Noise Cancellation back on show for a second (#113). So a report of the mode the last command replaced is held off until the mode set is reported, or 2 s pass - `aap::noise::ModeState`. After that any report wins again, since the mode may have been changed on another device.

Only the last command's replaced mode is held off, and that is accepted. A report of a mode an earlier command set or replaced is taken, so quick successive changes can briefly replay older modes as their echoes arrive, each about a second after its change on a Pro 3. The last command's echo comes last and settles it: the mode on show always ends as the one last set.

A report held off is dropped, not deferred to the end of the 2 s. In [run 1](../protocol/aap/noise-control.md#run-1-initial-validation), Transparency and Adaptive each took effect and were followed by a report of the old mode, `0x02`, with no echo of the new one after it: applied at expiry, that report would show the wrong mode just the same. If a dropped report was right after all, the next report corrects the mode.

## Unsolicited Mode Changes

The reports the pods send of a change made elsewhere: [protocol/aap/noise-control.md](../protocol/aap/noise-control.md#unsolicited-mode-changes).

Unsolicited reports are taken - except a report of the mode a command here replaced, within 2 s of it: see [Stale reports](#stale-reports).

## The pods switch the mode themselves

The pods' own switch to Transparency while the user speaks: [protocol/aap/conversation-awareness.md](../protocol/aap/conversation-awareness.md#the-pods-switch-the-mode-themselves).

The app takes them as it takes any report, so the window and the tray show Transparency while the user talks. That is the mode the pods are in.

A mode set from the app during a stretch is held as any command's is (`aap::noise::ModeState`): a report of Transparency within 2 s of it is taken for a stale one and dropped, and the next report - the echo, or the switch back after the speech - settles what is shown. What the pods do with a mode set during speech is not recorded; the fake pods go back to it after the stretch.
