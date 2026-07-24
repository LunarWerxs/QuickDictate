# Contributing to QuickDictate

Bug reports and pull requests are welcome. There are issue templates under
[.github/ISSUE_TEMPLATE](.github/ISSUE_TEMPLATE), and every PR runs the
[CI checks](.github/workflows/ci.yml). Maintainers cutting a release follow
the checklist in [docs/RELEASING.md](docs/RELEASING.md).

## Run CI locally before you push

`.github/workflows/ci.yml` runs on `windows-latest` and is a single job,
`fmt · clippy · build · test`:

1. `cargo fmt --all --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo build --release`
4. `cargo test`

`scripts/check.ps1` mirrors these gates so you can run them locally and get
the answer in a fraction of the time a GitHub runner takes:

```powershell
pwsh -File scripts\check.ps1          # fmt + clippy + test (fast, ~1 min warm)
pwsh -File scripts\check.ps1 -Full    # + release build, matches CI exactly
```

Run it before every push. Treat a green GitHub Actions run as confirmation
of a local pass, not as the first time you find out something is broken.

### `cargo fmt` is its own gate, separate from clippy

`cargo fmt --all --check` and `cargo clippy` are two different steps in CI,
and a clippy-clean tree is not necessarily fmt-clean. Hand-written or
lightly-edited Rust that never went through `cargo fmt` can pass clippy
locally and still turn CI red on the format-check step alone. Run
`cargo fmt --all` (or `scripts\check.ps1`, which checks both) before you
push, not just clippy.

### CI tracks latest stable Rust, not whatever you have installed

CI installs the toolchain via `dtolnay/rust-toolchain@stable`, which always
resolves to the current stable release at the time the workflow runs. If
your local `rustup` toolchain is behind, a clippy pass on your machine does
not guarantee a clippy pass in CI: new stable releases add new lints (for
example, a `question_mark` lint that a locally-green clippy run on an older
stable missed). Run `rustup update stable` before relying on a local clippy
pass, particularly if it's been a while since you last updated. If you have
more than one stable toolchain installed (e.g. both an MSVC and a GNU host
triple on Windows), make sure the one you're updating is actually the one
`cargo` picks up by default.

## Scope

Everything above covers the Rust gates CI enforces. There is no separate
lint/format toolchain for docs or scripts in this repo; keep pull requests
focused and consistent with the surrounding file's existing style.
