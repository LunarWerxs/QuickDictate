# Contributing to QuickDictate

Bug reports and pull requests are welcome. There are issue templates under
[.github/ISSUE_TEMPLATE](.github/ISSUE_TEMPLATE), and every PR runs the
[CI checks](.github/workflows/ci.yml). Maintainers cutting a release follow
the checklist in [docs/RELEASING.md](docs/RELEASING.md).

## Run CI locally before you push

`scripts/check.ps1` is the local mirror of `.github/workflows/ci.yml`. It runs
the same gates, in the same order, and answers in a fraction of the time a
GitHub runner takes:

```powershell
pwsh -File scripts\check.ps1          # everything below (~30s warm)
pwsh -File scripts\check.ps1 -Fast    # skip the supply-chain gates (inner loop)
pwsh -File scripts\check.ps1 -Full    # + release build and exe validation
```

| Gate | Command | CI job |
| --- | --- | --- |
| Format | `cargo fmt --all --check` | `check` |
| Lint | `cargo clippy --locked --all-targets -- -D warnings` | `check` |
| Tests (incl. fuzz) | `cargo test --locked` | `check` |
| Supply chain | `cargo deny check` | `deny` |
| Unused deps | `cargo machete` | `unused-deps` |
| Oldest compiler | `cargo +1.93 check` | `msrv` |

Install it as a pre-push hook once per clone, so a push that would go red is
stopped on your machine instead of leaving a red X on a public repo while you
find out:

```powershell
pwsh -File scripts\install-hooks.ps1
```

Bypass a hook once with `git push --no-verify` if you genuinely need to.

The two supply-chain tools are not in a default Rust install. `check.ps1` says
loudly when one is missing rather than quietly passing, but you should install
them:

```powershell
cargo install cargo-deny cargo-machete --locked
```

## The gates, and why each one is there

### `cargo fmt` is its own gate, separate from clippy

`cargo fmt --all --check` and `cargo clippy` are two different steps, and a
clippy-clean tree is not necessarily fmt-clean. Hand-written or lightly-edited
Rust that never went through `cargo fmt` can pass clippy locally and still turn
CI red on the format check alone.

### `unwrap` and `expect` are lints, not style preferences

`clippy::unwrap_used` and `clippy::expect_used` are warned at the crate root
(`src/main.rs`) and promoted to errors by `-D warnings`. This is not
fashion. A release build sets `windows_subsystem = "windows"`, so it has no
console: a panic on a background thread (audio capture, the hotkey pump, an STT
websocket worker, the settings-sync thread) writes to a stderr that goes
nowhere. The user sees dictation stop, with no error and nothing on screen. In
this app an `.unwrap()` is a **silent** failure, not a loud one.

Prefer `?`, `ok_or`, `unwrap_or`, or a `match`. Where a site genuinely cannot
fail, use a local `#[allow(clippy::unwrap_used, reason = "...")]` and make the
reason string the actual argument for why. Tests are exempt (see
`clippy.toml`): an unwrap in a test *is* the assertion.

### Fuzzing runs on every `cargo test`

`src/fuzz.rs` is a structure-aware mutation fuzzer over every parser that reads
untrusted input, wired in as ordinary tests rather than a separate job
specifically so it cannot be skipped. It costs about a second.

The threat model is worth being explicit about: QuickDictate parses **network
responses**. Speech-to-text provider frames, the AI-cleanup endpoint's replies
(that endpoint is user-configurable, so it points wherever they say), the
release-check payload that decides which binary gets downloaded and run, and
the settings-sync document. Each gets thousands of deterministic mutations plus
an exhaustive truncation sweep, because "the response was cut off mid-token" is
what a dropped connection produces for free.

Adding a parser that touches untrusted input? Add it to `targets()`. Adding a
new payload shape? Add a seed. `mutations_actually_reach_the_parsers` keeps the
suite honest by asserting that mutations still get past the UTF-8 and JSON
gates in bulk, so it can never quietly degrade into re-parsing valid input.

### Mutation testing, when you want to know if the tests have teeth

Not a CI gate (a full run is far too slow for one), but the honest answer to
"is this module actually covered". It disables one bit of behaviour at a time
and reports which changes NO test noticed:

```powershell
$env:CARGO_TARGET_DIR = "D:\qd-mutants"   # see below, this matters
cargo mutants --file src\paths.rs -j 1 --timeout 120
```

Scope it to the file you touched; a whole-crate run is a background chore, not
an iteration gate. Results land in `mutants.out\` (gitignored); `missed.txt` is
the list of gaps.

Two things to get right. **Set `CARGO_TARGET_DIR` to a scratch directory** so
mutant builds do not fight the real `target\` cache. And **never pass
`--in-place`** on a tree you have unsaved work in.

### `cargo deny` replaced `cargo audit`

`deny.toml` covers advisories *and* licenses, duplicate/wildcard versions, and
crate sources. That last one is the supply-chain question, and it matters here
because QuickDictate downloads and executes a native runtime at the user's
request (`local_stt`). Its extraction path has a dedicated traversal test,
`archive_extraction_cannot_escape_its_directory`.

`cargo deny` is also the one gate whose verdict changes with no code change, so
CI runs it weekly on a schedule as well as on every push.

### The toolchain is pinned

`rust-toolchain.toml` pins `stable-x86_64-pc-windows-msvc`. Before it existed,
CI and the release build used the MSVC host while a developer machine could
default to GNU: same `cargo build`, two backends, two linkers. Do not add a
toolchain action to the `check` job, and do not override the pin locally: that
is the drift it exists to remove.

The channel still floats on `stable`, so new stable releases still add new
lints. Run `rustup update stable` before relying on a local clippy pass if it
has been a while.

`Cargo.toml`'s `rust-version` is the oldest compiler CI actually builds
(the `msrv` job), not a guess about the oldest that might work. Raise it
deliberately.

## Scope

Everything above covers the Rust gates CI enforces. There is no separate
lint/format toolchain for docs or scripts in this repo; keep pull requests
focused and consistent with the surrounding file's existing style.
