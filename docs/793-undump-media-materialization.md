# Spec 793 — Undump Media Materialization (`<name>_media/` sidecar → real picker mount → purge)

**Status:** PROPOSED (2026-07-15). **Repo:** TRX64.
**Shared cross-repo numbering** (registry = C64RE `specs/README.md`).
**Depends on / touches:** Spec 707 (`.c64re` embedded `mediaPayloads`), Spec 709
(media ingress / dirty-media guard), Spec 714 (mutable media / file-backed mounts),
Spec 791 (VSF converter — the disk-extraction feeder), Spec 792 (restore fidelity —
the undump path this rides on).

## Problem — undump media is a phantom in-memory attach

Today an undump re-attaches embedded media as an **invisible in-memory attach**: the
drive8 disk bytes come straight out of the snapshot's `mediaPayloads` and are pushed
into `drive8`; the cart comes from `cartBytes`. It runs, but:

- **Invisible to the UI picker.** No file, no media-registry entry → the user cannot
  see what is mounted, cannot swap it, cannot inspect it. It is not a "real" mount.
- **No lifecycle.** The media just exists in RAM. The user cannot cleanly *abräumen*;
  an LLM test/overlay undump leaves nothing controllable to clean up.
- **Writes have nowhere to persist.** A resumed disk-writing game mutates an in-memory
  image with no backing file.
- **Asymmetric with a normal mount** — a user-mounted `.d64` is file-backed + in the
  picker; an undump-mounted one is neither.

**The fundamental gap (owner, 2026-07-15):** an undump should turn embedded media into
**real, visible, file-backed mounts the user owns** — identical to a normal mount — and
give a one-shot purge for the ephemeral (LLM/test/overlay) case.

## Design (owner's, 2026-07-15)

On undump of a `.vsf`/`.c64re` at `<dir>/<name>.<ext>`:

1. **Materialize** every embedded medium (disk **and** cart) as a real file under a
   **sibling folder** `<dir>/<name>_media/`:
   - drive8 disk → `<name>_media/<sourceName|drive8>.<d64|g64>`
   - cart → `<name>_media/<cartName|cart>.crt`
2. **Mount** each materialized file through the **normal media path** (Spec 709/714) so
   it is **file-backed** (writes persist to the file) and appears in the **UI picker
   exactly like a user mount**.
3. **The user (Alex) decides when to abräumen** — via the normal picker unmount/delete.
4. **LLM test/overlay case:** a **purge command** that kills all undump-materialized tmp
   media in one shot (auto-cleanup after a test run) — **without touching the user's own
   mounts**.

This replaces the phantom in-memory attach: after undump the machine runs off the
file-backed mount, and the media is a first-class, visible, owned artifact.

## Key rule — undump-materialized vs. user mounts (safety)

The daemon **tags** every mount it created by undump-materialization: a
`provenance = "undump-materialized"` marker + the owning `<name>_media/` dir. The purge
command removes **only tagged** media (unmount + registry entry + file). **A user's own
mount is NEVER touched.** Never delete what the user mounted — this is the hard safety
rule (mirrors the general "look before you delete / don't delete the user's work" law).

## What is NEW (this spec)

### 793.1 — materialize-on-undump (the mechanism)
The `.c64re` undump (monitor `undump` + WS `snapshot/undump`, the shared
`undump_native_snapshot` core) writes each embedded medium to `<name>_media/` and mounts
it **file-backed** via the runtime media registry, replacing the in-memory attach. Each
mount carries the `undump-materialized` provenance + its owning dir.

### 793.2 — picker visibility
The materialized mounts surface in `runtime_media_browse` / the UI picker as **normal
mounts** (source path = the `<name>_media/` file), with the `undump-materialized` marker
so the UI can badge them (and so the user knows which came from an undump).

### 793.3 — purge command
`undump_media_purge` (MCP tool + monitor `killmedia`): unmount + delete **every**
undump-materialized medium (default all; optionally scoped to one snapshot's
`<name>_media/`). Tag-scoped — refuses to touch a user mount. This is the LLM/test/
overlay auto-cleanup verb.

### 793.4 — cart externalization (refine decision 2026-07-15)
The cart is materialized as `<name>_media/<name|cartName>.crt` **and mounted**, so it is
a normal picker mount — swap / inspect / abräumen uniform with the disk. The cart bytes
**remain embedded** in the snapshot too (707 stays self-contained); the file is the
working/mountable copy.

## Non-goals

- **No `.c64re` format change** — 707 stays self-contained; `<name>_media/` is a working
  copy, not the snapshot's storage.
- **VSF DISK extraction** (VICE `DRIVE`/`GCR` → `.d64`/`.g64`) is **Spec 791.1c**. Until
  it lands, a VSF undump materializes only the **cart** (disk absent). 793 consumes
  whatever the undump path produces — it does not itself decode GCR.
- **No auto-delete on process exit** — the user owns `<name>_media/`; only the explicit
  purge (or a normal UI unmount/delete) removes it.
- Not a change to the dirty-media guard (709) — a materialized mount is a normal mount
  and obeys the same guard.

## Acceptance

1. Undump `WL.c64re` at `<dir>/WL.c64re` → `<dir>/WL_media/` contains the disk
   (`.d64`/`.g64`) **and** the cart (`.crt`); both appear in `runtime_media_browse` as
   mounts; the machine runs off the **file-backed** mounts (disk writes persist to file).
2. `undump_media_purge` unmounts + deletes `WL_media/` contents + registry entries; a
   **separately user-mounted** disk in the same session is **untouched**.
3. LLM test/overlay: undump → run → `undump_media_purge` leaves no tmp media, no leaked
   files.
4. VSF path: `convert-vsf` + undump materializes the EF **cart** as a mounted `.crt` in
   `<name>_media/` (disk pending 791.1c).

## Build order (slices)

1. **793.1** materialize + file-backed mount (the mechanism) on the `.c64re` undump path,
   with the `undump-materialized` provenance tag. Cart rides the same step (**793.4**).
2. **793.2** picker visibility (browse marker).
3. **793.3** purge command (MCP `undump_media_purge` + monitor `killmedia`), tag-scoped
   safety.
4. VSF wiring once **791.1c** extracts a disk — the same materialize step then also emits
   the `.d64`/`.g64`.
