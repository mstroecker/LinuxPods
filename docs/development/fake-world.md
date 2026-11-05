# The fake world

What the fake AirPods, the fake BlueZ and the fake media player do, and where they depart from the hardware.

## Ear detection

What the hardware sends: [protocol/aap/ear.md](../protocol/aap/ear.md).

The fake AirPods (`harness/pods.rs`) answer the notification request with the battery, then `0x08`, `0x0C`, `0x0E`, `0x2E`, `0x06` and `0x2E`, as capture 4 of #71 has them, and send a `0x06` on every change of a pod's place after that.

- On a lid change: the `0x08` with `SS` changed, then the `0x06`, then the battery.
- On a role switch: the `0x06` in the new order and the battery, then two `0x08`s - the first 10 ms later with `TT` set, the second 60 ms after it without. The hardware sends the battery between the two `0x08`s instead.
- A pod in the case is in the closed case the moment the lid closes, where the hardware took about 4.5 s.
- With both pods in the closed case, the pods drop the link.
- `SetInEar` and `SetInCase` of `FakeWorld1` move a pod over D-Bus, as `Pods::set_in_ear` and `Pods::set_in_case` do in the pipeline tests.
- A fake MPRIS player (`harness::player`) on the fake world's private session bus - `org.mpris.MediaPlayer2.fakeplayer` under `fakepods` - for the app to pause and play; what the user does in it goes through its `FakePlayer1.SetStatus`, and `Calls` lists every MPRIS method the app called.

## The volume lowered while the user speaks

What the app does: [app/volume.md](../app/volume.md).

The fake world plays this through: a fake BlueZ with A2DP transports, a call as A2DP going idle, a pair that answers a volume late and rounded if asked (`SetVolumeAnswer`), the user's volume, and fake pods that speak - see `.claude/skills/fake-world/SKILL.md` and the pipeline tests.

## Device information

The packet's layout: [protocol/aap/device-info.md](../protocol/aap/device-info.md#layout).

The fake pods send `0x1D` laid out so, fields 1-9 made up and field 10 `9442752` until a test changes it (`Pods::set_marketing_version`, `src/harness/pods.rs`), so the fake world shows the row.

## Rename

The fake pods take a rename (`src/harness/pods.rs`).
