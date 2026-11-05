# The desktop

What the desktop around BlueZ does that a client has to know: PipeWire following the transport's volume, and the path a press on a pod takes to a player.

## The volume

The transport's `Volume`: [bluez.md](bluez.md#mediatransport1volume).

Checked on a Pro 3 on 2026-11-01 (#150): the transport's `Volume` set from 32 to 20 and back with `busctl`, and PipeWire's sink followed within 0.5 s (25% to 16% to 25%). GNOME's slider, which follows the sink, was not watched.

## A press on a pod

As we understand it - not traced here - a press on a pod travels as an AVRCP passthrough that bluetoothd turns into a key on an input device of its own, `<name> (AVRCP)`, which GNOME's media keys service hands to the one player it takes as current over MPRIS.

Why the app does not go this way to pause the media: [app/media-pause.md](../../app/media-pause.md).

## Open questions

- A press on a pod: confirm the path above - `evtest` shows the key, and the MPRIS player changes state.
