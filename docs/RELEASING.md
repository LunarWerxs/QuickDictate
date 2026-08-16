# Releasing QuickDictate

Maintainer checklist for cutting a release. Versions live in more places than
Cargo.toml: this list exists so none of them drift (SECURITY.md sat on
`0.1.x` until `0.3.0` shipped).

## 1. Bump the version

- [ ] `Cargo.toml`: set `version = "X.Y.Z"` (Cargo.lock refreshes on the next build).

## 2. Update the docs that name a version

- [ ] `CHANGELOG.md`: move the `[Unreleased]` items into a new `## [X.Y.Z] - YYYY-MM-DD` section, leaving `[Unreleased]` empty at the top.
- [ ] `.github/SECURITY.md`: "Supported Versions": update the "(currently the `X.Y.x` line)" note and the `X.Y.x (latest release)` table row.
- [ ] `.github/ISSUE_TEMPLATE/bug_report.md`: refresh the example version in the "QuickDictate version" comment.
- [ ] Sweep for stragglers referencing the *previous* version (ignore CHANGELOG history and test fixtures in `src/update.rs`):

  ```powershell
  Get-ChildItem -Recurse -Include *.md,*.toml -Exclude CHANGELOG.md | Select-String '<old version>'
  ```

## 3. Verify

- [ ] `pwsh -File scripts\check.ps1 -Full`: the exact gates CI runs. Format,
      Clippy with `-D warnings`, the whole test suite (including the
      mutation-fuzz pass over every parser that reads a network response, and
      the archive-traversal red-team test), `cargo deny`, `cargo machete`, the
      release build, and the exe validation.
- [ ] `pwsh -File scripts\smoke_test.ps1`: the only check that exercises the real
      pipeline end to end (hotkey capture, audio routing, provider connect, paste).
      CI cannot run it: it needs a microphone, a focused window, and a live API key.
      Everything the release workflow verifies is static (PE metadata, fmt, Clippy,
      unit tests), so a regression in dictation itself ships undetected without this.
- [ ] Dictate once by hand into a normal editor and once into a long-text target
      (>80 characters, which takes the clipboard path), then confirm the clipboard
      still holds whatever you had copied beforehand.
- [ ] `cargo deny check`: refresh the advisory database and resolve every
      compatible patched version into `Cargo.lock`. (`check.ps1` above already
      runs it; called out separately because it is the one gate whose verdict
      can change with no code change at all.)

## 4. Tag and publish

- [ ] Commit, tag `vX.Y.Z`, and push the tag.
- [ ] The `Release` GitHub Actions workflow reruns formatting, Clippy, tests, and
      the locked release build from that exact tag. It then verifies the public
      executable's GUI subsystem, version metadata, embedded icon, and
      side-effect-free `--version` canary before publishing `quickdictate.exe`,
      `SHA256SUMS.txt`, and the matching CHANGELOG section.

  The direct `quickdictate.exe` is intentionally both the human download and
  auto-update payload. A ZIP reduces the roughly 12 MB binary to about 6 MB, but
  that modest absolute saving is not worth adding extraction and migration
  complexity to the running dictation app; every already-published updater also
  expects this stable exact filename. The updater requires GitHub's asset
  SHA-256 digest and verifies the downloaded PE, byte count, digest, and reported
  version before swapping it into place.

  There is exactly **one** build now: every provider is compiled in
  unconditionally. Before `0.4.3` the Google provider sat behind a Cargo
  feature, which meant two different binaries shared one filename and the
  release could (and did) ship the wrong one. If you ever reintroduce a
  feature gate, this step needs a "which build?" answer again.

## Dependency security

Run `cargo deny check` before every release and resolve any compatible patched
version into `Cargo.lock`. CI runs it on every push and weekly on a schedule,
because it is the only gate whose verdict can go red with no code change: the
advisory database moves under a lockfile that did not.

`deny.toml` at the repo root is the config. It **replaced `.cargo/audit.toml`**
(deleted): `cargo audit` answered only "is anything in the lockfile
vulnerable", while `cargo deny` answers that plus three questions nothing here
was asking:

- **licenses**: every dependency's license must be on the allowlist. A new
  crate arriving under a surprising one fails the build instead of shipping.
- **bans**: duplicate versions and wildcard requirements are surfaced.
- **sources**: dependencies must come from crates.io. No git remotes are
  allowed; adding one is a deliberate edit to `allow-git`. This is the
  supply-chain question, and it matters here specifically because QuickDictate
  downloads and executes a native runtime at the user's request (`local_stt`).

The old ten-entry advisory ignore list is **gone, not migrated**. Every entry in
it was an advisory reachable only through a non-Windows target (the GTK tray
backend, Wayland window decorations), and `deny.toml`'s `[graph] targets` pins
the two Windows triples so those crates leave the graph entirely rather than
being waved through. `[advisories] ignore` is now empty and should stay that
way: an ignore list nobody can explain is how a real vulnerability hides among
irrelevant ones.

If `cargo deny` reports something new:

- **A reachable advisory**: resolve a compatible patched version into
  `Cargo.lock` (`cargo update -p <crate>`).
- **A genuinely unreachable one with no fix**: verify with
  `cargo tree --target x86_64-pc-windows-msvc -i <crate>` (and the `-gnu`
  triple) that no dependency path exists, then add it to `[advisories] ignore`
  with a **date** and the reason it cannot be patched. Re-verify such entries
  each release rather than assuming they still hold: a dependency bump can make
  a previously target-gated crate reachable again.
- **A new license**: add it to `[licenses] allow` only after checking it
  permits redistribution in a binary.
