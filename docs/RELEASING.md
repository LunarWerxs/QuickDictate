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

- [ ] `pwsh -File scripts\check.ps1 -Full`: the exact gates CI runs, including release builds.
- [ ] `pwsh -File scripts\smoke_test.ps1`: the only check that exercises the real
      pipeline end to end (hotkey capture, audio routing, provider connect, paste).
      CI cannot run it: it needs a microphone, a focused window, and a live API key.
      Everything the release workflow verifies is static (PE metadata, fmt, Clippy,
      unit tests), so a regression in dictation itself ships undetected without this.
- [ ] Dictate once by hand into a normal editor and once into a long-text target
      (>80 characters, which takes the clipboard path), then confirm the clipboard
      still holds whatever you had copied beforehand.
- [ ] `cargo audit`: refresh the advisory database and resolve every compatible
      patched version into `Cargo.lock`.

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

Run `cargo audit` before every release and resolve any compatible patched
version into `Cargo.lock`.

The accepted advisory ignore list now lives in `.cargo/audit.toml`
(`[advisories] ignore = [...]`), each entry commented with the dependency
path and affected target that make it unreachable on Windows. `cargo audit`
reads that file automatically, so a clean run means every advisory is either
fixed or already accounted for there.

You only need to touch this section of the checklist when `cargo audit`
reports something **not** already in `.cargo/audit.toml`:

- If it's a genuinely new, reachable advisory: resolve a compatible patched
  version into `Cargo.lock`.
- If it's genuinely unreachable (a target-gated platform not compiled into
  the Windows binary) and no compatible fix exists: verify with
  `cargo tree --target <triple> -i <crate>` for both
  `x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc`, then add it to
  `.cargo/audit.toml` with a comment recording the dependency path,
  affected target, and rationale.

Re-verify every existing entry in `.cargo/audit.toml` on each release instead
of assuming it still holds: a dependency bump could make a previously
target-gated crate reachable again.
