# Releasing

One release = one tag, one workflow dispatch. The workflow builds all six platform
archives, publishes the GitHub release and then bumps the Homebrew tap.

1. Bump `version` in `Cargo.toml` and the "as of" line in `docs/FEATURES.md`.
2. `cargo build` (refreshes `Cargo.lock`).
3. Commit: `release: X`.
4. `git push origin main`.
5. Tag and push the tag: `git tag vX && git push origin vX`.
6. Push the Gitea mirror: `git push gitea main vX`.
7. Write the notes to `notes.md`, then dispatch:

   ```sh
   gh workflow run release-binaries.yml --ref main -f version=X \
     -f title="TRX64 X" -f notes="$(cat notes.md)"
   ```

   `title` defaults to `TRX64 X`; `notes` defaults to empty and can be changed later with
   `gh release edit vX --notes-file notes.md`.
8. Watch it: `gh run watch`. Nothing else to do.

What happens after the dispatch:

- `release-binaries` builds the six targets. The `publish` job runs only if **all** of
  them are green. It checks that the six archives and six `.sha256` files exist, that
  tag `vX` exists and points at the dispatched commit, and that no release `vX` exists
  yet. It then creates a draft, uploads the 12 files and publishes it as latest. A
  failure leaves no release behind.
- `tap-bump` follows automatically when `release-binaries` completes green: it reads
  the tag from the run's `release-tag` artifact and commits the new version, URLs and
  checksums to `Jondalar/homebrew-trx64`.

Dispatching without `version` only builds (artifacts kept 14 days) and publishes
nothing. If the tap bump must be repeated: `gh workflow run tap-bump.yml -f tag=vX`.
