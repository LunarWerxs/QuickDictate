# Changelog

All notable changes to QuickDictate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Mouse buttons can be hotkeys.** The middle button and the two thumb buttons
  (the ones usually labelled Back and Forward) can now drive dictation, on their
  own or with modifiers: `mouse3`, `mouse4`, `mouse5`, or e.g. `ctrl+mouse4`.
  Record one exactly as you record a key, by clicking the dot in the hotkey field
  and pressing the button.

  Previously this was impossible in three separate ways, all fixed: the recorder
  only listened for key presses and ignored mouse buttons entirely; the combo
  parser had no names for them; and Windows' `RegisterHotKey`, which every
  hotkey rode on, is keyboard-only and cannot bind a mouse button at all. Mouse
  bindings now take a different route, a low-level mouse hook, which re-arms
  itself on the same one-minute timer that keeps the keyboard hotkeys alive
  through sleep, session locks, and RDP reconnects.

  A bound mouse button is **claimed**: it stops reaching whatever is under your
  cursor, so a thumb button bound to dictation no longer also navigates your
  browser back. Set `"mouse_hotkey_passthrough": true` to share the button
  instead of claiming it. Modifiers must match exactly, as they do for keyboard
  hotkeys, so binding plain `mouse3` leaves Ctrl+middle-click alone.

  Left and right click are deliberately not bindable: claiming one would
  suppress it system-wide, including the clicks needed to get back into Settings
  and undo it.

  Mouse hotkeys also answer to *injected* clicks, not just physical ones. That
  sounds like a detail and is the difference between the feature working and
  silently doing nothing in two common setups: a Remote Desktop session, where
  every click is delivered by the RDP stack rather than a local device, and a
  mouse whose vendor driver (G HUB and friends) remaps a button by synthesizing
  it. QuickDictate only ever injects keystrokes, never mouse input, so there is
  no feedback loop to guard against here.

  When a bound button is pressed but the held modifiers don't match, the log now
  says so, with both the expected and the actual modifiers. A stuck Ctrl or
  Shift (which RDP sessions produce routinely) otherwise makes a correctly
  configured hotkey look dead for no visible reason.

  Note that Remote Desktop cannot carry the two thumb buttons at all: its base
  protocol has left/right/middle and the wheel, and the extension that adds the
  others is not sent by most clients, including the mobile apps. Bind and test
  thumb buttons at the physical machine. See docs/GUIDE.md.

- **Pick which microphone to record from**, with the new `input_device` setting.
  Empty (the default) keeps the old behaviour of following the Windows default
  recording device; any part of a device name pins that one (`"yeti"`).
  Matching is case-insensitive, and a named device that isn't present falls
  back to the default rather than failing, because an absent microphone must
  never be the reason dictation stops working.

  Useful for dictating from another machine, with one caveat worth stating
  plainly: an app can only record a microphone that exists on the machine it
  runs on. Your voice reaches the PC only if the remote-desktop tool publishes
  your local mic there as an audio input device. Microsoft's client does when
  you enable it; RustDesk and Chrome Remote Desktop do not forward the client
  microphone at all. When such a device exists, name it here like any other.
  There is deliberately no transport detection or per-product special-casing:
  the substring match already covers every case, and no amount of
  session-sniffing can make absent audio appear.

- **The running capture now follows microphone changes.** Previously the stream
  was rebuilt only after it *failed*, so a device that merely appeared next to a
  perfectly healthy one was never switched to, and a mid-run mic change needed a
  restart. It now re-checks every couple of seconds and switches over on its
  own, logging the change; a swap is not treated as a fault, so the error pip
  stays quiet.

## [0.8.0] - 2026-08-15

### Added

- **Choose where QuickDictate keeps its files.** Until now the `logs\` folder,
  the usage stats, the settings-sync credential blob, and the update cache were
  all written next to the executable. That is fine when the exe has its own
  folder and unpleasant when it does not: an exe kept on the Desktop turned the
  Desktop into a scratch directory. **Settings ▸ Application ▸ Files** now
  points them anywhere, with **Use AppData** as a one-click preset
  (`%LOCALAPPDATA%\QuickDictate`) and **Next to the app** to go back. Existing
  files are moved across on the next start, and nothing is ever overwritten.

  Equivalent to the new `data_dir` key in `settings.json` (`%VARIABLES%` are
  expanded; the path must be absolute). The environment variable
  `QUICKDICTATE_DATA_DIR` overrides both, for scripted portable installs.

  Moving the folder a second time works too: the folder in use is recorded in
  `%LOCALAPPDATA%\QuickDictate\active-data-dir.txt`, so going from one custom
  location to another carries the stats and sync credentials along instead of
  stranding them in the folder being abandoned.

  `settings.json` itself stays where it is, since it has to be found before it
  can be read, but it is now also looked for in `%LOCALAPPDATA%\QuickDictate` so
  the executable's own folder can be left completely empty. `data_dir` is a path
  on one PC, so it is never synced.

### Changed

- **`cargo deny` replaced `cargo audit`.** It checks the same advisories plus
  dependency licenses, duplicate and wildcard versions, and crate sources.
  Config moved from `.cargo/audit.toml` to `deny.toml`, and the old ten-entry
  advisory ignore list is gone rather than migrated: every entry was reachable
  only through a non-Windows target, and pinning the Windows triples removes
  those crates from the scanned graph outright.
- **The toolchain is pinned** (`rust-toolchain.toml`,
  `stable-x86_64-pc-windows-msvc`), matching CI and the release build. A
  developer machine defaulting to the GNU host was building a different thing
  from the one that ships. `Cargo.toml` now also declares a tested `rust-version`.
- CI gained `deny`, `msrv`, and `unused-deps` jobs; `scripts\check.ps1` mirrors
  all of them, and `scripts\install-hooks.ps1` wires it in as a pre-push hook.

### Fixed

- **`webbrowser` updated to 1.2.4** (RUSTSEC-2026-0257, browser argument
  injection via the `BROWSER` variable). Not exploitable here (the flaw is in
  the crate's Unix path and QuickDictate is Windows-only), patched anyway.
- **Panics that would have been invisible.** `clippy::unwrap_used` and
  `clippy::expect_used` are now enabled crate-wide and enforced by CI. A release
  build has no console, so a panic on a background thread stops dictation with
  no error and nothing on screen. Every site was either rewritten to return an
  error or annotated with the reason it cannot fire; the ones rewritten include
  the local-STT engine handle, its GPU-to-CPU retry path, the Google provider's
  retry loop, and a mutex in the About window that could carry another thread's
  panic into a dead window.

### Security

- **The parsers that read untrusted network responses are now fuzzed on every
  test run** (`src/fuzz.rs`). Speech-to-text frames, the AI-cleanup endpoint's
  replies, the release-check payload that decides which binary gets downloaded
  and executed, and the settings-sync document each take thousands of
  deterministic mutations plus an exhaustive truncation sweep, inside
  `catch_unwind`. Wired in as ordinary tests so it cannot be skipped.
- **The local-STT archive extractor has a traversal test.** The downloaded
  runtime is a `.tar.gz` unpacked to disk; a crafted entry escaping it would be
  an arbitrary file write. Relative `..` walks, absolute paths, and Windows
  drive-qualified paths are all proven not to escape.

## [0.7.2] - 2026-08-13

Everything below shipped as one release. 0.5.7 through 0.7.1 were version bumps
made while building it and were never published, so they are folded in here
rather than left as five same-day entries for releases nobody could install.

### Fixed

- **Pausing mid-sentence no longer starts a new sentence.** Every streaming
  provider commits a segment whenever you pause, and ElevenLabs Scribe writes
  that pause out as a trailing "...". Two rules then treated it as a full stop:
  the sentence-capitalization regex read the last dot of the ellipsis as a
  sentence end, and the hybrid paste flow processed each post-release commit as
  its own standalone sentence, capitalizing its first word regardless of what
  came before. So one spoken thought with two breaths in it came back as "so I
  don't want to... Significantly slow down the process." An ellipsis is now read
  as a pause, and a chunk following an unfinished one keeps its lower-case
  opening. Continuation is scoped to a single hotkey press, so the next
  dictation still opens a fresh sentence; a real "." or "?" and even "?!"
  capitalize exactly as before. No network, no added latency.

### Added

- **Optional AI cleanup pass** (Settings -> Application -> "Clean up with AI
  before pasting"; off by default). It repairs the sentence boundaries a pause
  made the recognizer invent, plus obviously misheard words, and nothing else.
  Two things keep it off the critical path: while you are still talking the
  held transcript is unpasted, so the pass runs in that free time and is usually
  **already answered** by the time you release; when it is not, the paste waits
  at most `polish_deadline_ms` (default 300 ms) and pastes unpolished if the
  answer misses. A waiting paste attaches to the request already in flight
  rather than starting a fresh one, so that budget only has to cover what is
  left of it. Speculation also has a growth floor, since every pass except the
  last is discarded by construction and a stop-start dictation would otherwise
  fire one full request per pause.
- The model returns an **edit list**, not a rewritten transcript: ~10x fewer
  output tokens, and every edit must quote its target verbatim and unambiguously
  or the whole set is discarded. An edit set changing more than a quarter of
  what you said is refused outright, measured on what actually differs rather
  than on how much context the model quoted to stay unambiguous. No key, no
  network, a malformed reply, or a model that tried to rewrite you all fall back
  to the text that would have been pasted anyway.
- `polish_endpoint` takes any OpenAI-compatible chat-completions URL. Measured
  against a real dictation, **gemini-3.5-flash-lite answered in ~0.56 s with the
  most complete edit set**, against ~2.0 s for gpt-4.1-mini; the full table is
  on `default_polish_model`. The *lite* tiers win outright here, since this is a
  small mechanical edit rather than a reasoning problem, and the slowest models
  tested were the two that insist on thinking first. Keys live in the same key
  manager every provider uses, several are rotated to spread per-project rate
  limits, and "Test all" probes the cleanup endpoint rather than the speech
  provider (a Google key can be valid and still be rejected by it). Per-app
  profiles take a `"polish"` override, which is how you keep it out of terminals
  and editors. Everything but the key syncs.

### Changed

- **Deepgram now does its own formatting.** `smart_format` / `punctuate` were
  never being sent, so it returned unpunctuated lower-case text that our own
  rules then had to guess at. The same fixture went from "the quick brown fox
  ... testing one two three four five" to "The quick brown fox ... Testing
  12345." Note the number handling there: spoken digit runs get merged, which is
  right for a phone number and wrong if you were counting.
- `QUICKDICTATE_UI_PAGE=dictation|history` opens the settings window straight to
  a page, so the headless screenshot hook can capture any of them.

## [0.5.6] - 2026-08-10

### Fixed

- **No more red "!" after a dictation you never spoke into.** Start a
  dictation, say nothing, stop it, and ElevenLabs closes the socket without a
  closing handshake. That reset was being reported as a failed dictation even
  though there was no transcript to lose, so an empty press occasionally
  flashed the error pip for nothing. The pip is now raised only when speech
  really was lost: the transcript already landed (no error), the provider
  returned no words at all (an empty press, so also no error), or the socket
  died mid-press and cut us off (still an error). Session logs now also record
  how many chunks were above the silence floor, next to the chunk totals.

## [0.5.5] - 2026-08-09

### Changed

- **The Settings window has a nav rail instead of one long scroll.** Three
  pages, following SageThumbs 2K's layout: **Application** (provider and keys,
  app behavior, settings sync), **Dictation**, and **History**. The window went
  from roughly 1160 points tall to a fixed 760x600.
- **"Log full dictated text" is greyed out unless logging is on**, instead of
  looking like an active privacy choice with nothing to write into.

### Fixed

- **Resizing the Settings window no longer fights back.** It measured its
  content every frame and pushed its own size to the OS; dragging an edge
  rewrapped the content, which changed the measured height, which snapped the
  window to a new size mid-drag, so it appeared to jump open and shut and
  change width on its own. It now keeps whatever size you give it.

## [0.5.4] - 2026-08-09

### Added

- **Custom vocabulary.** A list of words and phrases QuickDictate sends to the provider so it
  gets names, jargon, and product names right in the first place, instead of repairing them
  afterwards with the text-replacement fix-list. Wired into each backend's own biasing
  parameter; providers without one ignore it.
- **Per-app profiles can now override language, provider, and vocabulary**, not just text
  handling. A profile naming a provider with no configured key falls back to the global one, so
  a typo cannot leave you unable to dictate.
- **Transcript history browser** in Settings, with search, copy, and paste-again. Previously the
  history was only reachable through a small tray submenu.
- **`protect_keys_at_rest`** (off by default) encrypts the API keys in `settings.json` with
  Windows DPAPI. Off by default because it costs portability: a sealed file will not decrypt on
  another PC or another Windows account.
- Per-app profiles, the profiles master switch, voice commands, and the custom vocabulary now
  travel with Connections settings sync. A new test fails the build if a future `Config` field is
  neither synced nor explicitly marked machine-local, which is how these four came to be missed.
- The tray tooltip and the cursor pip now name the actual failure: out of credit, rate limited,
  network unreachable, keys rejected, elevated window, or a hotkey another app has claimed.
  Every adapter already computed this; it was being collapsed into a bare "!".

### Fixed

- **A replacement value containing `$` was silently corrupted.** Replacement text went through
  regex capture-group expansion, so a rule producing `$50` typed nothing for the amount.
- **Replacement rules could cascade into each other.** Rules were applied one after another over
  the growing output in alphabetical key order, so one rule's output could trigger another. They
  now all apply in a single pass over the original text, longest pattern first.
- **The clipboard is no longer destroyed by a paste.** Text, files, images, HTML, RTF, and PNG
  are snapshotted and restored, not just plain text, so copying an image, a file list, or an
  HTML fragment and then dictating no longer loses it. (Deliberately not every format: fetching
  the exotic ones an Excel or browser copy advertises forces the source app to render them on
  the spot, which froze the paste.) The restore now also runs on every failure path and on a
  panic; previously an error after the clipboard was emptied lost its contents permanently.
- **A long dictation is no longer lost when the clipboard is busy.** If another app is holding
  the clipboard, the paste falls back to keystrokes, and every transcript now enters the history
  even when the paste fails, so "recent transcriptions" can always recover it.
- **"Scratch that" no longer backspaces into the wrong window.** It refuses to fire if focus
  moved since the paste it would undo, and it counts grapheme clusters rather than Unicode
  scalars, so a multi-codepoint emoji no longer over-deletes.
- **Held modifier keys no longer corrupt the paste.** A modifier-based hotkey left Ctrl or Alt
  physically down while text was injected, turning Ctrl+V into Ctrl+Alt+V and typed characters
  into menu accelerators.
- **Typing into an elevated window no longer reports success.** Windows discards injected input
  from a lower integrity level and `SendInput` still claims every event was sent. QuickDictate
  now detects this, leaves the text on the clipboard, and says so.
- **A dropped connection mid-dictation is no longer reported as a clean finish.** It was
  indistinguishable from a normal end of stream: no retry, no error, and uncommitted speech
  silently gone.
- **The last spoken segment is no longer dropped after an earlier sentence committed.** The
  fallback that promotes an unfinalized trailing partial was gated on a session-wide flag, so it
  switched itself off permanently after the first successful commit.
- **A stalled network during dictation now surfaces instead of hanging.** `send_audio` had no
  timeout in the live phase, so a blackholed connection froze the session until the user let go.
- **Google: one transient error no longer discards the rest of the recording** or benches a
  healthy API key. Failed segments retry with backoff, and only genuine credential failures
  touch the key pool.
- **A microphone change mid-dictation no longer corrupts the audio.** Sessions in flight kept
  resampling at the old device's rate and channel count after a reopen.
- **DashScope now honours an explicitly selected language.** It was computed and never sent.
  The app-wide default ("en-US") is still NOT sent, so configs that never touched the language
  setting keep Paraformer's auto-detect instead of being pinned to English.
- **`shutdown()` could block for up to 60 seconds** on a stale hotkey thread-id snapshot.
- **Saving no longer freezes the Settings window** for up to six seconds on a slow network.
- **"Default settings" now asks for confirmation** before wiping every setting, and closing
  Settings with unsaved edits warns instead of discarding them silently.
- **Toggle and Hold can no longer be saved to the same key**, which silently disabled one of the
  two dictation modes.
- A portable build no longer adopts an unrelated `settings.json` found in a parent folder, which
  it would then overwrite on the next save.
- The panic log honours the `enable_logging` opt-in like every other log file.
- The daily update check now stamps its cache even when it fails, so an offline machine stops
  making the network round-trip on every launch.
- A final release now correctly supersedes its own release candidate; the prerelease suffix was
  stripped before comparison, so `1.0.0` looked identical to `1.0.0-rc1`.
- Log lines identify a failing key by its position in the list rather than by the last six
  characters of the credential.
- The cursor pip reacts to the hotkey instantly again while the app still idles quietly: the
  overlay thread now wakes on status changes and window messages instead of polling, so both
  the earlier 100 ms poll and the brief laggy 1 s version are gone.
- A provider dropping the connection right after delivering the final transcript no longer
  flashes the error pip; transport errors only surface when the session delivered nothing.
- A dictation error before the first successful session names its real cause instead of
  inheriting a stale "out of credit" from startup key probing.
- Opening Settings on a synced machine no longer shows a false "unsaved changes" prompt after
  the silent cloud pull, and Save can no longer revert a just-pulled vocabulary.
- Settings sync no longer refuses to push when a synced text field happens to contain a
  32/40-character hex string (a git commit id, an MD5 hash); only unambiguous credential
  shapes are blocked.
- A brief disk hiccup during a background sync-token refresh retries instead of signing the
  account out.

### Changed

- **An available update is reported, not installed.** Through v0.5.3 the daily check silently
  downloaded, verified, swapped the executable, and relaunched. The download URL and its
  SHA-256 both come from the same release payload, so the hash proves the bytes match what was
  uploaded, not that anyone intended to upload them. Clicking the About pill is now the consent.
  Set `update_auto_install` to restore the old behaviour. See SECURITY.md.
- The local model unloads after ten minutes idle instead of holding several GB of RAM and VRAM
  for the whole uptime of the tray app.
- The overlay no longer wakes ten times a second while idle, and no longer creates and destroys
  a GDI font on every repaint.
- The audio capture callback no longer allocates per chunk.
- Release CI pins every third-party GitHub Action to a commit SHA, `cargo audit` also runs on
  pull requests that change the lockfile, and the accepted advisory list moved from a manual
  step in RELEASING.md into `.cargo/audit.toml`.

## [0.5.3] - 2026-07-27

### Fixed

- **Log files can no longer grow without a real bound.** Rotation kept a single backup, so a
  long-running session at a verbose level could sit at roughly twice the configured cap forever.
  It now keeps four numbered generations, drops the oldest, and enforces a total on-disk budget
  even when one oversized write lands whole.

### Changed

- Dependency monitoring runs on its own weekly schedule in CI, and Dependabot watches both the
  crate graph and the workflow actions.

## [0.5.2] - 2026-07-26

### Changed

- **Connections sync is safer across devices and accounts.** Nested preferences merge without
  replacing unrelated remote changes, cache validators stay tied to the active account, pending
  settings flush during shutdown, and server throttling follows its requested retry delay.
- **The release is a normal icon-bearing Windows GUI executable.** It starts without an
  accompanying console, and the same direct executable remains the established self-update
  payload so existing installations keep updating without an archive migration.

### Fixed

- Settings larger than the 64 KiB Locker limit are rejected using their actual UTF-8 byte size
  instead of character count, so non-ASCII settings cannot slip past the client-side guard.

## [0.5.1] - 2026-07-24

### Added

- **Private usage statistics.** A new **Stats** window shows lifetime words, dictated audio time, dictation count, speaking pace, longest dictations, and a provider breakdown. Only numeric aggregates are stored locally in `quickdictate-stats.json`; transcript text and API keys are never included.
- **Bulk API-key import.** **Manage keys** now has a **Bulk add** button beside **Add**. Paste one key per line to trim blank lines, reject malformed entries, and import only new keys without disturbing the existing order.

### Changed

- **Diagnostics now have their own folder.** Active, rotated, and panic logs live under `logs\`, and the Settings overflow action is now **Open log folder**. Existing root-level logs are migrated to collision-safe legacy filenames instead of being overwritten.

### Fixed

- **Long Cohere dictations no longer collapse into repeated phrases.** Cohere audio is split at quiet boundaries into clips no longer than 35 seconds, with a shorter retry and conservative repetition guard if a clip still degenerates.
- **Timed-out sessions cannot paste a second late result.** Provider-specific final-result grace periods preserve slower complete transcripts, finalization tasks are cancelled before a partial is promoted, and a dropped phantom final cannot return through the partial fallback.
- **Local inference no longer leaves an unused microphone subscriber alive.** The final resampler fragment is flushed before the audio receiver closes, then the subscription is removed before batch inference begins.

## [0.5.0] - 2026-07-24

### Added

- **Fully offline transcription.** The new Local provider needs no API key and keeps microphone audio on the PC. Choose between Cohere Transcribe 03-2026 Q5_K_M (1.65 GiB, the accuracy-first default) and Whisper Large v3 Turbo Q5_K_M (591 MiB, smaller with broad language coverage).
- **One-click local model management.** Settings can install, select, cancel an active download, or delete either model. Weights are never embedded in the executable or repository: they download on demand to Local AppData, are pinned to immutable upstream revisions, and are verified by exact size and SHA-256 before becoming usable.
- **Purpose-built Local status feedback.** Because offline transcription runs as a final batch rather than producing live partials, the cursor pip now shows an animated spinner instead of a frozen zero-word counter.

### Changed

- **Local model downloads are substantially faster and safer.** QuickDictate uses up to eight parallel HTTP range workers when supported, falls back cleanly to a single stream, removes incomplete files after cancellation or interruption, and shares one small native runtime between installed models.
- **Local startup latency is paid in the background.** The selected model loads and prewarms when Local is selected, remains resident between dictations for predictable response time, switches automatically with the model selector, and releases its RAM/VRAM when you switch to a cloud provider.
- **Long-running memory and idle work are bounded.** Microphone queues now have fixed capacity, Google batch recognition uploads ordered 55-second blocks instead of retaining an entire long recording, logging has a bounded lossy queue and rotates during a run, clipboard/avatar/update buffers have size limits, and the cursor/tray loop polls less often while idle.

### Fixed

- **Cold Local results no longer appear to do nothing.** Starting another dictation while the first Vulkan inference was still initializing could supersede and discard a valid transcript. Final Local processing is now serialized, an early hotkey press queues the next dictation, and queued hold-to-talk starts are cancelled if the key is released before processing finishes.
- **Save & Restart returns to Settings.** The replacement process now reopens the Settings window automatically, and a failed relaunch leaves the current process running with a visible error instead of silently closing QuickDictate.
- **Audio buffers are no longer duplicated at chunk boundaries.** The microphone callback now feeds each captured buffer into each session exactly once.
- **Non-BMP Unicode characters paste correctly.** Characters such as emoji are emitted as complete UTF-16 surrogate pairs instead of being truncated.
- **Live settings and update paths are more robust.** Provider/key changes refresh the active key pool, failed update downloads clean up partial files, and bounded network/file handling prevents stalled external work from retaining unbounded memory.

## [0.4.3] - 2026-07-15

### Changed

- **Google Cloud Speech-to-Text is now in every build.** It used to sit behind a `--features google` build switch, so the Google provider only existed if you compiled it in yourself. It's now always included, like the other five providers, and the Settings provider list always offers it. Nothing to enable: paste a key into `google_keys` and pick Google.
- **`0.4.2` accidentally shipped without the Google provider.** The switch meant one source tree could produce two different `quickdictate.exe` files, and the wrong one was published. If you use Google and updated to `0.4.2`, this release restores it. Removing the switch retires that whole class of mistake: there is only one binary now.

## [0.4.2] - 2026-07-15

### Added

- **"Hide tray icon" is now in the tray's right-click menu.** Tucking QuickDictate out of sight no longer means opening Settings to find the checkbox. It asks for confirmation first and spells out the way back (launch QuickDictate again and Settings reopens), since hiding the icon also hides the menu you'd use to unhide it.

## [0.4.1] - 2026-07-13

### Changed

- **Updates now install themselves, silently.** When a newer release is found, QuickDictate downloads it, verifies it (executable header, size, and SHA-256), swaps the exe in place, and relaunches into the new version with no prompt. The background check keeps its once-a-day throttle, and if you're mid-dictation the relaunch waits until you're idle (the new version applies on your next restart) so an update never interrupts you. After a manual update from the About window it reopens About on the new version (instead of a pop-up notice); a silent background update stays fully silent.

### Added

- **The error pip now explains a dead-key failure.** When a dictation fails because every configured API key was rejected (invalid or unauthorized), the red pip shows a key glyph instead of a bare "!", and the tray icon's hover text says your API keys were rejected and to open Settings to update them, staying until dictation works again.

### Fixed

- **The About window's update chip now updates the app instead of opening GitHub.** When an update is waiting, clicking the "Update to …" pill downloads and installs it in-app (the same verified swap-and-relaunch as the background updater) rather than sending you to the releases page.
- **A self-update or "Save & Restart" can no longer leave QuickDictate closed.** When the app relaunches itself, the incoming process now reliably takes over from the outgoing one instead of occasionally mistaking it for a duplicate launch and exiting, which in a timing-dependent race could shut the app down entirely.

## [0.4.0] - 2026-07-13

### Fixed

- **Dictation no longer tacks on a short answer you never said.** After you stop talking, QuickDictate keeps listening briefly to catch trailing words; that trailing silence used to be sent to the provider, and some models (notably ElevenLabs Scribe) would "complete" the dead air with a hallucinated reply. It now holds silent audio back and forwards it only if you resume speaking, for any tail length.
- **Long silences no longer drop the live transcription.** Streaming providers get a lightweight keep-alive during a quiet tail, so a long "keep listening" window stays connected.
- **The Settings window no longer scrolls or leaves dead space.** It sizes to fit its content exactly, at any zoom or window state.
- **The Save split-button's dropdown matches the Save button.**

### Changed

- **Settings layout tidied.** Per-app profiles moved into the Application card's right column, a more compact "Text replacements" button, and tighter spacing.

## [0.3.0] - 2026-07-09

### Changed

- **Connections sync now shows your display name and avatar** instead of a bare email, fetched from the auth backend's userinfo endpoint.

### Fixed

- **Hardened failure paths across the audio and STT layers.** Capture-stream death is now surfaced and the default device is reopened on a retry loop instead of dying silently; a press aborts with a visible error pip when audio is down; live provider connects are capped (10 s overall, 6 s DashScope handshake) and rotate keys on timeout so a silent-but-open socket can't park a session.
- **Corrupt or unwritable settings no longer fail invisibly.** An unparseable `settings.json` is backed up to `settings.json.bad` and reported, saves are atomic (write-then-rename), and audio-init / settings alerts now show a message box instead of vanishing into a log under `windows_subsystem = "windows"`.
- **A pathological transcript can't take down the output thread.** Text processing runs behind `catch_unwind`, so a bad transcript costs one paste, not the whole output path.

## [0.2.0] - 2026-07-07

### Added

- **Two timing levers in Settings → Dictation.** **Hold to re-paste** sets how long you hold the toggle hotkey to replay your last dictation (was a fixed 1.5 s; applies after a restart). **Keep listening after you stop** sets how long QuickDictate keeps capturing after you stop talking before it finalizes, the "dynamic tail" silence window (was a fixed 0.8 s; applies on your next dictation). Both are sliders shown in seconds, sync with your other portable prefs, and default to the previous fixed values so behavior is unchanged until you move them.
- **Optional "Sync settings with Connections."** A new opt-in card in Settings signs you in with a free Connections account (system-browser OAuth with PKCE, no password ever touches the app) and syncs your **portable preferences** (hotkeys, provider, text replacements, toggles) across every machine you use QuickDictate on. **Your API keys never sync**, only an allowlist of non-secret prefs travels, and the refresh token is sealed with Windows DPAPI. No new dependencies. Details: `docs/SETTINGS_SYNC.md`.

### Changed

- **Log file no longer grows without bound.** `quickdictate.log` is a single file appended across every launch; it now rotates aside to `quickdictate.log.old` at startup once it passes a size cap (`max_log_mb`, default **5 MB**; `0` disables). Machine-local, not synced.
- **Settings-sync card is more compact.** The signed-in row drops the "as <account>" text and shows the sync status inline next to the green **Synced** badge instead of on a separate line below.
- **Settings window is ~10% smaller** (a uniform zoom, it read a touch oversized).
- **Primary actions moved to a pinned bottom bar:** **About** at the bottom-left, **Save** / **Save & Restart** at the bottom-right, which also removes the empty padding that used to sit below the buttons.
- **Bottom bar tidied up.** The loose "Check for updates / Open log file / Edit settings.json" button row is now a single **⋯ overflow menu** next to About, and the two Save buttons became one **split button**: **Save** with a small **▾** that drops down **Save and restart**.
- **Dictation timing knobs are now compact, inline controls.** "Hold to re-paste" and "Keep listening after you stop" used to be two long full-width sliders; they're now a plain seconds text box each (type the value, no click-and-drag), with a small "s" unit label, laid out label-left / control-right in two columns to match Language, Mode, and the hotkey fields above them. The divider that used to sit above them is gone, so they tuck directly under the hotkey block as one group and the card is shorter.
- **Record-hotkey dot shows a pointer cursor.** Hovering the little "record" dot in a hotkey field now switches the cursor to a pointing hand, so it reads as clickable.
- **Per-app profiles folded into the Application card.** The "Enable per-app profiles" toggle now sits with the other Application toggles instead of in its own near-empty section; the read-only profile list only appears when you've actually added `profiles` to settings.json.
- **Roomier modals.** The Text replacements (and API keys) pop-ups got more left/right padding so their fields no longer hug the edges.
- **Tray "Recent transcriptions" now copies to the clipboard.** Clicking a past transcription in the tray submenu puts it on the clipboard for you to paste yourself, instead of auto-pasting it into whatever window happens to be focused.
- **About box opens centered over Settings.** The About window now appears centered on the Settings window it was opened from, instead of always the center of the primary monitor (it still falls back to screen-center if the Settings window can't be located).

### Fixed

- **Dictation no longer pastes old/stale clipboard text.** For longer dictations (which paste via the clipboard), QuickDictate briefly put your text on the clipboard, pressed Ctrl+V, then restored your previous clipboard after only 60 ms. But the keystroke is only *queued*, a slower app (browsers, Electron apps) often read the clipboard after the restore and pasted the **old** contents instead, and that stale text got re-parked on your clipboard after every long dictation. The restore delay is now a configurable **300 ms** (`clipboard_restore_delay_ms`, `0` = don't restore), and the restore is skipped entirely if another app wrote the clipboard in the meantime, so it can never clobber a fresh copy.
- **Hotkeys no longer die after "Save & Restart."** Global Windows hotkeys are exclusive to one process, so the relaunched app could fail to grab the hotkey while the old instance was still exiting, and the old code treated that as fatal, killing the hotkey thread until you manually restarted again. Startup registration is now non-fatal and retries for a few seconds (invisible handoff), falling back to the periodic self-heal re-arm if needed.
- **Settings re-opens every time now (and no longer disturbs the hotkey).** Opening Settings, closing it, and opening it again used to do nothing, the window stayed shut, and could also leave the global dictation hotkey unresponsive. Root cause: the window's UI toolkit only allows one event loop per process, so tearing it down on close permanently blocked re-creating it. The window now **hides** on close and re-shows on the next open (re-seeded to a clean state), so Settings opens reliably and closing it no longer tears down anything that the hotkey path could get caught on.
- **First run with no API keys now opens Settings for you.** Previously it only showed a pop-up telling you to go open Settings yourself and then did nothing. Now the Settings window opens automatically, with a pinned **"Add an API key to get started"** banner at the top (with a one-click **Manage keys…** button) that disappears the moment you save a key for any provider. The old separate warning pop-up is gone, the auto-opened window carries the message instead.

## [0.1.7] - 2026-07-04

### Changed

- **~21% smaller download** (13.6 MB → 10.7 MB): HTTPS now uses the OS-native TLS backend (schannel) instead of bundling a second full rustls + Mozilla-CA stack, and the release binary is fully symbol-stripped. No behavior change, the update-check and Google STT paths were re-verified over schannel.

### Added

- Unit-test coverage for the core pure-logic paths: the text processor (spacing / punctuation / capitalization / dev-term and replacement handling), the hotkey combo parser + virtual-key lookup table, and per-provider key resolution. (68 tests, up from 53.)

### Fixed

- Docs: the SECURITY.md vulnerability-disclosure channel no longer has an unfilled email placeholder (now points to a private GitHub Security Advisory); README links the changelog; corrected a stale "not yet live-verified" note on the OpenAI adapter (it's verified).

## [0.1.6] - 2026-07-04

### Changed

- Settings → Speech-to-text provider: **Manage keys… and Test all keys now sit on the dropdown's row** (one row shorter).
- Settings → Dictation: the **Record buttons are gone**, each hotkey field now has a small, subtle record dot tucked into its right edge (click it, then press a key). The two input halves are laid out independently so neither can squeeze the other.
- Settings → Application: the four toggles are now in **two columns**.
- The **Text replacements…** button no longer stretches full-width, it sizes to its label.

## [0.1.5] - 2026-07-04

### Added

- **Enable text replacements** toggle in Settings, a master on/off switch for the whole replacement pass (the saved list is kept, just not applied while off).
- The **Check for updates** flow now shows a spinning arc for at least ~2 seconds before the result lands, so the check reads as actually doing something instead of flashing past.

### Changed

- Settings → Dictation is now laid out as a grid: a 2×2 block of labeled inputs (Language / Mode / Toggle hotkey / Hold hotkey) over two columns of checkboxes.
- All text fields and dropdowns in Settings share one control height, so inputs, dropdowns and buttons line up.
- Removed the redundant "N key(s) configured" line from the provider card (the Manage keys… modal already shows the keys and their status).

## [0.1.4] - 2026-07-04

### Added

- **Record hotkey**: a "Record" button next to each hotkey field in Settings, click it and press a key/combo to set the hotkey.
- **Bulk text-replacements editor**: the Text replacements modal has a "Text editor" toggle that shows all replacements as `from => to` lines, so a big set can be pasted/copied at once.

### Changed

- The tray menu is now minimal (version, Settings…, Open Executable Location, Quit). **About**, **Check for updates** (opens the About window with the live version status), **Open log file**, and **Edit settings.json** moved into the Settings window.
- Fixed the "Save && Restart" button showing a double ampersand, now "Save & Restart".

## [0.1.3] - 2026-07-04

### Added

- Auto-default provider: if the configured `stt_provider` has no keys but another provider does, the app opens straight into that provider (so pasting only, say, Google keys just works). An explicit `--provider` is always respected.
- The settings template is now **baked into the exe** (`include_str!` of settings.example.json); on first run, when no settings.json exists, it's written out from that template, no separate settings.example.json file shipped alongside.
- `scripts/check.ps1`: local CI, runs the exact fmt/clippy/build/test gates GitHub CI runs, so you can verify a change in ~1 minute instead of waiting on GitHub.

### Changed

- Empty-key onboarding notice is now **provider-agnostic** ("No API keys found" instead of naming ElevenLabs), QuickDictate works with any provider.
- Updated the settings window to **egui/eframe 0.35** (from 0.31); the key/text-replacement modals now use egui's native `Modal`.

## [0.1.2] - 2026-07-04

### Added

- **Settings window** (tray → "Settings…"): provider dropdown, API-key manager in a modal (masked keys, add/remove, per-key status chips, "Test all" probing every key **in parallel** against the real provider API), text-replacements editor modal, hotkey fields with validation, and all the common toggles, styled to the LunarWerx look (brand-blue rounded checkboxes and buttons, carded sections, Segoe UI, dark/light theme). `settings.json` stays the source of truth; "Edit Settings (JSON)" remains in the tray for advanced fields.
- Headless UI screenshots for development: `QUICKDICTATE_UI_SHOT=<png>` makes the settings window capture itself via egui's viewport screenshot (`scripts/ui_shot.ps1` wraps the loop; `-Open keys-test` also runs a live parallel key test before capturing).

## [0.1.1] - 2026-07-04

### Added

- Key prewarm (`prewarm_keys`, default on): the active provider's keys are probed in the background at startup; dead/limited keys are pre-marked and the first validated key is queued ready for the first dictation.
- `--provider <id>` command-line override for `stt_provider`, plus a `QuickDictate-Launcher.bat` menu for launching with any of the six providers.
- Dev-trigger `about` command (opens the About window without the tray).

### Changed

- Key health now lives in memory only, `key-health.json` is gone. Every launch starts fresh and re-probes, so a temporarily limited key or a provider outage never permanently brands a key dead. Failed keys cool down (duration scaled to the failure kind) and become eligible again automatically.
- About window rebuilt as a faithful port of the LunarWerx "2026" card: owner-drawn version + update-status pills (GitHub mark, live status dot), theme-aware dark/light skin with dark titlebar, per-monitor DPI scaling, LunarWerx Studios wordmark, hand cursors over clickables.

### Fixed

- A key that failed at the connection stage (e.g. DashScope reporting an account in arrears) aborted the whole dictation with a red "!" instead of rotating to the next key. Connect failures now rotate within the same press.

## [0.1.0] - 2026-07-03

### Added

- Multi-provider speech-to-text support: ElevenLabs (Scribe v2 realtime), Deepgram (nova-3), OpenAI (gpt-4o-transcribe via the GA Realtime API), AssemblyAI (Universal-Streaming v3), Alibaba Cloud DashScope Paraformer (paraformer-realtime-v2), and Google Cloud Speech-to-Text (batch v1).
- Bring-your-own-key model: each provider has its own key array in `settings.json`, supporting multiple keys with round-robin selection and per-key health tracking (alive / quota / dead) plus cooldown backoff.
- Toggle and hold hotkey modes for starting/stopping dictation (`toggle_hotkey` / `hold_hotkey`, defaults `f14` / `f13`).
- Hybrid delayed-paste policy (`delay_output_till_release`) for controlling when recognized text is typed.
- Text replacements setting for correcting commonly misheard phrases.
- First-run notice (popup + log entry) when no API key is configured for the selected provider.
- DashScope region toggle (`dashscope_intl`) to select between the mainland-China host (default) and the International host.
- Google Cloud STT batch provider gated behind the optional `google` cargo feature.
- Live provider test harness (`#[ignore]`d integration tests) for exercising real provider APIs locally with real keys.
- Continuous integration workflow covering `cargo fmt`, `clippy`, build, and test.
- Check-for-update + portable self-update: daily-throttled startup check (`update_auto_check`, default on) and a tray "Check for Updates…" item; downloads are verified (MZ header + size + SHA-256) and the exe is swapped in place after confirmation, then relaunched.
- "About QuickDictate" tray item: version, live update-check status, MIT license, © 2026 Lunarwerx, clickable LunarWerx Studios credit.
- Self-healing global hotkeys: re-registered every minute so dictation survives sleep/resume, session lock, and RDP reconnects.
- `run_at_startup` setting: start QuickDictate at Windows login (per-user Run key, no admin rights).
- "Open Log File" tray item.
- Embedded VERSIONINFO resource (company/product/version metadata) to reduce AV/SmartScreen false-positive heuristics on the unsigned exe.
