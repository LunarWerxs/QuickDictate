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
- [ ] **Rebuild the asset explicitly with default features:**

  ```powershell
  cargo build --release        # NOT --features google
  ```

  Read this even if `check.ps1 -Full` just passed. Its last gate is
  `cargo build --release --features google`, so it leaves the **google** build
  sitting in `target/release\quickdictate.exe` — uploading "whatever's in
  target/release" ships a binary that contradicts
  [providers.md](providers.md), which tells users the Google provider is
  source-only. That is exactly how `v0.4.1` shipped google-enabled while
  `v0.3.0` / `v0.4.0` did not. To confirm what you're about to upload:

  ```powershell
  # expect NO match for a default build
  Select-String -Path target\release\quickdictate.exe -Pattern 'speech.googleapis.com'
  ```

- [ ] Publish the GitHub release with the release notes (the CHANGELOG section for this version) and that `quickdictate.exe`. The in-app update check relays GitHub's release info, so the update prompt goes live as soon as the release is published — and since updates now install **silently**, shipping the wrong variant changes what's on users' machines without them noticing.
