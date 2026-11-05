# Commands

How a command reaches a pair: the one path every command takes, the order it keeps, and
what its caller hears. The commands are the noise control mode
([noise-control.md](noise-control.md)), Conversation Awareness
([conversation-awareness.md](conversation-awareness.md)) and the rename
([names.md](names.md#rename)). The key request is no command: the link sends it as it
comes up ([keystore.md](keystore.md)).

## One path

Every command - the window's and the tray's - goes through
`Coordinator::command(mac, Command)`, which names its pair.

**Queued in the call itself, not at the first poll** (#54). The command takes its place
on the link's queue when `command` is called, so commands go out in the order of the
calls, however their futures are spawned or awaited. Spawned apart, two quick switches
went out in either order, and the mode recorded last could be the one sent first.

Nor does sending wait for anything: `command` can be called from the GTK main context,
which no tokio runtime runs on, and its future awaited there.

**One at a time, with the link.** `AapLink::run` carries the commands out in turn beside
the read loop ([aap-links.md](aap-links.md#running-and-ending)). So they go with the
link wherever its run stops, and none can outlive it to send on a newer link's behalf.
What a command records goes through `Inner::live_link`.

## The queues

The queues (`Coordinator::queues`) are a std mutex beside `inner`, so a command is queued
without waiting for anything.

- **Taken only under `inner`'s write lock, or alone.**
- **Never held across an await.**
- **Changed only together with `Inner::links`,** so a pair has a queue exactly while it
  has a link, and it is that link's.
- **Removed only past `drop_link`'s client check,** by its own link, so a newer link's
  queue stands.

## What the caller hears

Each caller gets a `Result`:

| | The result |
|---|---|
| A name `aap::name` refuses | An error, at once; nothing is queued ([names.md](names.md#rename)) |
| No link to the pair, or its link going | An error, at once |
| The send failed | An error |
| The link went before the command was sent | An error: not carried out |
| The link went in the middle of the command | `ConnectionLost`: the packet may or may not have gone out |
| Sent | `Ok`, once the command has recorded what it set |

**Dropping the future does not withdraw the command.** Once queued, it is carried out.

The window shows a failure as a toast ([ui.md](ui.md#updates-come-from-snapshots)); the
tray logs it.

## Hardware checks

None open: the commands themselves are checked in their own documents.
