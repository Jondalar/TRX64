# Spec 837 — A paused machine still has a picture

**Status:** BUILT 2026-09-10 — `cargo test -p trx64-daemon` 178/0, incl. the new
`a_client_arriving_while_paused_gets_a_frame`; the C64RE half ships with it.
**Number:** 837 (shared board `C64ReverseEngineeringMCP/specs/README.md`).
**Origin:** Issue #13 — a cockpit stuck on "No frame yet — emulator booting…"
after an `undump`, never recovering, while every other reading said the machine
was healthy. The reporter's only known cure was killing the daemon, which drops
the shared session and forces a full replay.

## 1. What is actually wrong

`streaming.rs` renders and broadcasts inside `if running`. **A paused machine
therefore sends no frame at all.** The loop starts on the first subscriber and
stops when the last one leaves, so a client that ARRIVES during a pause — a page
reload, or the loop restarting after the last client had gone — starts a loop
that renders nothing, and the placeholder it shows before its first frame stays
up for ever.

The reporter's guess, that the RAM/cart divergence warning was the cause, is
wrong: that warning is correct and harmless. `undump` is simply a common way to
end up paused, and its own test asserts that it does.

What already existed and is NOT the gap: `force_present_frame`, a one-shot the
paused branch consumes. `undump` and `checkpoint/restore` both set it, after a
2026-07-15 regression of exactly this shape. Every OPERATION that leaves a
paused machine asks for a present. **Nothing asked on arrival** — and a client
that was not there when the operation happened never learns what the picture is.

## 2. Decision

`StreamHub::subscribe` sets `force_present_frame`. A new client gets one frame
whether or not the machine is running, because a paused machine has a picture —
it just is not changing. One present on arrival, not a stream: rendering a still
image fifty times a second would be the other kind of wrong.

Reusing the existing one-shot rather than adding a second mechanism is
deliberate. There was a strong pull to add a `repaint` flag to the hub; the
paused branch already had exactly this concept, and a second one would have
meant two answers to "why did a frame appear while paused".

## 3. What this does not fix

Nothing here detects a stream that stops while the machine keeps running. That
is the C64RE half: the workbench already knew (its fps counter reads zero while
the run-state says running) and said nothing, so a dead picture looked like a
slow boot. It now says which of the two states it is in and offers the way back.
