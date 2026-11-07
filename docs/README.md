# Documentation

Two kinds of document live here, and they are kept apart: what the AirPods and the
Linux Bluetooth stack do, in `protocol/`, and what LinuxPods does about it, in `app/`.
`development/` holds what is needed to work on LinuxPods rather than to understand it,
and `user/` what someone using it needs.

## Which side a fact goes on

**Would it still be true if LinuxPods did not exist?** Then it goes in `protocol/`. If
it is true because of a choice made here, it goes in `app/`.

| `protocol/` | `app/` |
|---|---|
| The pods drop what they are sent until they have answered the handshake; a pair that had just connected answered after 1.2 s (406c721). | `Client::handshake` returns only once the answer is in. |
| A `0x0D` report can carry the mode a change replaced: a Pro 3 switched to Off reported the old mode 0.33 s later (#113). | A report of the replaced mode is held off until the mode set is reported, or for at most `STALE_REPORT_WINDOW`. |
| The pods keep 127 bytes of a name and cut a longer one silently (Pro 3, #160). | A name over 127 bytes is refused before it is sent, never cut by bytes. |
| bluetoothd answers `StartDiscovery` with `InProgress` to a client already discovering, and also when the kernel refuses the start as busy (BlueZ 5.87, `src/adapter.c`). | The scanner stops its discovery and starts again. |
| Each pod advertises from a resolvable private address of its own, a new one every 4-8 minutes (Pro 3 and Pro 2 (USB-C), #71). | An advertisement counts only if a stored IRK resolves its address and that pair's key decrypts it; a pair with no IRK yet is tried by its key alone. |

A fact and the choice made on it are two sentences in two places. Where a document
has both today, it is split, and each half links to the other.

## `protocol/`

What AirPods send and take, and how the Linux stack under the app behaves. Written so
that someone building another client can use it without reading our code.

- `aap/` - the Apple Accessory Protocol over L2CAP: the session, keys, battery, ear,
  noise control, the `0x09` settings, Conversation Awareness, device information, rename
- `ble/` - Apple's proximity pairing advertisements, and the addresses they come from
- `linux/` - the kernel's L2CAP and LE scanning, bluetoothd, zbus, and the desktop around
  them (MPRIS, PipeWire, UPower): what they do that a client has to know

Names from our code - types, constants, functions - stay out of these documents, but
for one line at the top pointing at the parser and at the `app/` document that uses
the fact.

The runs and captures a fact rests on go in the same document, below the facts they
support. Each document ends with **Open questions**: what is not known yet.

## `app/`

How LinuxPods behaves and why: what it does with each fact, the tradeoffs it accepts,
and the bugs that shaped it, by issue number. Written for whoever changes the code
next.

Each document ends with **Hardware checks**: what has run only against the fake world,
and how to try it on a pair.

## `development/`

Releases, the fake world and where it departs from the hardware, and the probes in
`examples/` that the protocol runs were made with.

## `user/`

For people who use LinuxPods, not those who change it: a problem they may meet, how to
tell it is theirs, and what to do. The app links some of these on GitHub, so a file
here keeps its name.

- **Plain words, and no names from our code** - no types, constants, functions or log
  lines. What the user sees in the app is named as the app shows it.
- **The steps are written out; the reason is linked.** A fact about the AirPods or the
  Linux stack is linked to the `protocol/` or `app/` section that has it, with its
  source, not restated: that link is where rule 2 is met.
- **No Open questions or Hardware checks.** What is not known yet stays in the document
  linked.

## Rules for every document

### Content

1. **One fact, one place.** Everywhere else links to it. A copy is what goes stale: it
   is never updated with the original.
2. **Every fact says where it comes from:**
   - a run here: the model, the date, and the issue or commit - "Pro 3, 2026-11-01,
     #150"
   - another project, named and linked - "LibrePods says" - until a run here confirms
     it, and then the run
   - a specification or an upstream source, named with its version and file - "Core
     spec Vol 3 Part H 2.2.2", "BlueZ 5.87, `src/adapter.c`"
   - a guess, called one

   "Assumed for the Pro 2, not checked" is a fact of this kind. What one model did is
   never written as what all of them do.
3. **Dates go on facts, not on files.** No "Last updated" or "Tested with" footer: they
   go stale on their own.
4. **Describe what is true now.** A fact that changed is corrected where it stands, and
   its history stays in the issue and the commit. The exception is a warning that keeps
   a mistake from being made again - "do not validate with magic bytes" - which keeps
   its reason and its issue.
5. **Unknowns go in one list,** Open questions or Hardware checks, at the end. Not in
   passing, where nobody looking for what is left to try will find them.

### Against going stale

6. **No code that copies the implementation.** Show a packet as a byte layout or a
   table. For working code, point at `examples/`, which CI compiles.
7. **In `app/` and `development/`, name a constant rather than repeat its value.** "Held off for
   `STALE_REPORT_WINDOW`" stays true when the constant changes; "held off for 2 s" does
   not. Give the value as well only where the reasoning depends on it.
8. **A change to behavior changes its document in the same pull request.**
9. **Links are relative**, to a file or to a heading in it, so a link checker can
   follow them.

### Privacy

10. **No keys, serial numbers, MAC addresses, host addresses or personal device
    names** - an address captured from a device is left out even when it is a random
    one. Use placeholders, as the BLE samples do. A capture goes in as a short,
    redacted excerpt, with the command that produced it, so that it can be run again.

### Writing

11. **Open with what the document covers and how sure it is,** in two or three lines.
    Then the facts, then the evidence.
12. **Lead with the point, one claim per sentence.** Several conditions are a table or a
    list, not one long sentence. Many readers of these documents do not read English
    natively.
13. **US English** (color, behavior), plain words, and one convention for byte offsets,
    stated once at the top of each document that uses them.

## Code comments

A comment says what the code beside it does and why, and links to a document here for
the evidence. Content rules 1-4 and 6-8, the privacy rule and plain US English hold for
comments too. The rules below are the ways comments went wrong anyway: a review found
about 45 such comments in `src/` and `examples/` (#185) after #58 and #133 had brought
them in line once, among them two guesses about noise control that stood as fact in
four files after their document called them guesses.

1. **A protocol fact is linked, not restated.** A comment that needs a fact about the
   AirPods or the Linux stack points at the `protocol/` section that has it. A comment
   that states the fact all the same carries the status the document gives it -
   "LibrePods says", "a guess", "open" - and changes when the document does. Where
   the code and a document disagree, the comment takes neither side: it says the
   question is open and points at the document's Open questions or Hardware checks
   entry on it, and the document gets one if it has none.
2. **No history.** A comment says what the code does, not what it replaced: no "the Go
   version", "used to", "as before", "the old way". History stays in the issue and
   the commit. A comment that names the bug a line guards against, with its issue
   number, gives a reason, not history.
3. **Only what the code beside it makes true.** A claim about other code - what another
   module sends, who calls a function, what a library emits - goes unseen when that
   code changes. Name the item, so that a search for it finds the comment, or leave the
   claim out. A guarantee a comment states - a lock held, an order kept - holds only
   if the code under it enforces it.
4. **The long explanation goes in `app/`.** A comment on a choice `app/` explains is a
   line or two and a link: a module doc that retells its `app/` document drifts from
   it. The doc comments of the probes and the fakes are the reference `development/`
   points at, and stay whole.
5. **A test's doc says what the test asserts.** A change to the asserts rewrites the doc
   in the same edit.

## Where else

- `README.md` at the root says what the app does, for users: features, installing,
  usage. It links here for how, and to `user/` for a problem a user may meet.
- `AGENTS.md` holds the rules for working in this repository: one line per invariant,
  with a link to the `app/` or `protocol/` document that says why, or to the rule here
  it follows.
- Code comments say what the code does and link here for the evidence: see
  [Code comments](#code-comments).
