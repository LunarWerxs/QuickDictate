# Releasing QuickDictate

Maintainer checklist for cutting a release. Versions live in more places than
Cargo.toml — this list exists so none of them drift (SECURITY.md sat on
`0.1.x` until `0.3.0` shipped).

## 1. Bump the version

- [ ] `Cargo.toml` — set `version = "X.Y.Z"` (Cargo.lock refreshes on the next build).

## 2. Update the docs that name a version

- [ ] `CHANGELOG.md` — move the `[Unreleased]` items into a new `## [X.Y.Z] - YYYY-MM-DD` section, leaving `[Unreleased]` empty at the top.
- [ ] `.github/SECURITY.md` — "Supported Versions": update the "(currently the `X.Y.x` line)" note and the `X.Y.x (latest release)` table row.
- [ ] `.github/ISSUE_TEMPLATE/bug_report.md` — refresh the example version in the "QuickDictate version" comment.
- [ ] Sweep for stragglers referencing the *previous* version (ignore CHANGELOG history and test fixtures in `src/update.rs`):

  ```powershell
  Get-ChildItem -Recurse -Include *.md,*.toml -Exclude CHANGELOG.md | Select-String '<old version>'
  ```

## 3. Verify

- [ ] `pwsh -File scripts\check.ps1 -Full` — the exact gates CI runs, including release builds.

## 4. Tag and publish

- [ ] Commit, tag `vX.Y.Z`, push the tag.
- [ ] Publish the GitHub release with the release notes (the CHANGELOG section for this version) and the `target\release\quickdictate.exe` that `check.ps1 -Full` just built. The in-app update check relays GitHub's release info, so the update prompt goes live as soon as the release is published.

  There is exactly **one** build now — every provider is compiled in
  unconditionally. Before `0.4.3` the Google provider sat behind a Cargo
  feature, which meant two different binaries shared one filename and the
  release could (and did) ship the wrong one. If you ever reintroduce a
  feature gate, this step needs a "which build?" answer again.
