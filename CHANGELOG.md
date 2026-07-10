# Changelog

All notable changes to QuickDictate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.3.0] - 2026-07-09

### Changed

- **Connections sync now shows your display name and avatar** instead of a bare email, fetched from the auth backend's userinfo endpoint.

### Fixed

- **Hardened failure paths across the audio and STT layers.** Capture-stream death is now surfaced and the default device is reopened on a retry loop instead of dying silently; a press aborts with a visible error pip when audio is down; live provider connects are capped (10 s overall, 6 s DashScope handshake) and rotate keys on timeout so a silent-but-open socket can't park a session.
- **Corrupt or unwritable settings no longer fail invisibly.** An unparseable `settings.json` is backed up to `settings.json.bad` and reported, saves are atomic (write-then-rename), and audio-init / settings alerts now show a message box instead of vanishing into a log under `windows_subsystem = "windows"`.
- **A pathological transcript can't take down the output thread.** Text processing runs behind `catch_unwind`, so a bad transcript costs one paste, not the whole output path.

## [0.2.0] - 2026-07-07

### Added

- **Two timing levers in Settings → Dictation.** **Hold to re-paste** sets how long you hold the toggle hotkey to replay your last dictation (was a fixed 1.5 s; applies after a restart). **Keep listening after you stop** sets how long QuickDictate keeps capturing after you stop talking before it finalizes — the "dynamic tail" silence window (was a fixed 0.8 s; applies on your next dictation). Both are sliders shown in seconds, sync with your other portable prefs, and default to the previous fixed values so behavior is unchanged until you move them.
- **Optional "Sync settings with Connections."** A new opt-in card in Settings signs you in with a free Connections account (system-browser OAuth with PKCE — no password ever touches the app) and syncs your **portable preferences** (hotkeys, provider, text replacements, toggles) across every machine you use QuickDictate on. **Your API keys never sync** — only an allowlist of non-secret prefs travels, and the refresh token is sealed with Windows DPAPI. No new dependencies. Details: `docs/SETTINGS_SYNC.md`.

### Changed

- **Log file no longer grows without bound.** `quickdictate.log` is a single file appended across every launch; it now rotates aside to `quickdictate.log.old` at startup once it passes a size cap (`max_log_mb`, default **5 MB**; `0` disables). Machine-local, not synced.
- **Settings-sync card is more compact.** The signed-in row drops the "as <account>" text and shows the sync status inline next to the green **Synced** badge instead of on a separate line below.
- **Settings window is ~10% smaller** (a uniform zoom — it read a touch oversized).
- **Primary actions moved to a pinned bottom bar:** **About** at the bottom-left, **Save** / **Save & Restart** at the bottom-right — which also removes the empty padding that used to sit below the buttons.
- **Bottom bar tidied up.** The loose "Check for updates / Open log file / Edit settings.json" button row is now a single **⋯ overflow menu** next to About, and the two Save buttons became one **split button**: **Save** with a small **▾** that drops down **Save and restart**.
- **Dictation timing knobs are now compact, inline controls.** "Hold to re-paste" and "Keep listening after you stop" used to be two long full-width sliders; they're now a plain seconds text box each (type the value — no click-and-drag), with a small "s" unit label, laid out label-left / control-right in two columns to match Language, Mode, and the hotkey fields above them. The divider that used to sit above them is gone, so they tuck directly under the hotkey block as one group and the card is shorter.
- **Record-hotkey dot shows a pointer cursor.** Hovering the little "record" dot in a hotkey field now switches the cursor to a pointing hand, so it reads as clickable.
- **Per-app profiles folded into the Application card.** The "Enable per-app profiles" toggle now sits with the other Application toggles instead of in its own near-empty section; the read-only profile list only appears when you've actually added `profiles` to settings.json.
- **Roomier modals.** The Text replacements (and API keys) pop-ups got more left/right padding so their fields no longer hug the edges.
- **Tray "Recent transcriptions" now copies to the clipboard.** Clicking a past transcription in the tray submenu puts it on the clipboard for you to paste yourself, instead of auto-pasting it into whatever window happens to be focused.
- **About box opens centered over Settings.** The About window now appears centered on the Settings window it was opened from, instead of always the center of the primary monitor (it still falls back to screen-center if the Settings window can't be located).

### Fixed

- **Dictation no longer pastes old/stale clipboard text.** For longer dictations (which paste via the clipboard), QuickDictate briefly put your text on the clipboard, pressed Ctrl+V, then restored your previous clipboard after only 60 ms. But the keystroke is only *queued* — a slower app (browsers, Electron apps) often read the clipboard after the restore and pasted the **old** contents instead, and that stale text got re-parked on your clipboard after every long dictation. The restore delay is now a configurable **300 ms** (`clipboard_restore_delay_ms`, `0` = don't restore), and the restore is skipped entirely if another app wrote the clipboard in the meantime, so it can never clobber a fresh copy.
- **Hotkeys no longer die after "Save & Restart."** Global Windows hotkeys are exclusive to one process, so the relaunched app could fail to grab the hotkey while the old instance was still exiting — and the old code treated that as fatal, killing the hotkey thread until you manually restarted again. Startup registration is now non-fatal and retries for a few seconds (invisible handoff), falling back to the periodic self-heal re-arm if needed.
- **Settings re-opens every time now (and no longer disturbs the hotkey).** Opening Settings, closing it, and opening it again used to do nothing — the window stayed shut — and could also leave the global dictation hotkey unresponsive. Root cause: the window's UI toolkit only allows one event loop per process, so tearing it down on close permanently blocked re-creating it. The window now **hides** on close and re-shows on the next open (re-seeded to a clean state), so Settings opens reliably and closing it no longer tears down anything that the hotkey path could get caught on.
- **First run with no API keys now opens Settings for you.** Previously it only showed a pop-up telling you to go open Settings yourself and then did nothing. Now the Settings window opens automatically, with a pinned **"Add an API key to get started"** banner at the top (with a one-click **Manage keys…** button) that disappears the moment you save a key for any provider. The old separate warning pop-up is gone — the auto-opened window carries the message instead.

## [0.1.7] - 2026-07-04

### Changed

- **~21% smaller download** (13.6 MB → 10.7 MB): HTTPS now uses the OS-native TLS backend (schannel) instead of bundling a second full rustls + Mozilla-CA stack, and the release binary is fully symbol-stripped. No behavior change — the update-check and Google STT paths were re-verified over schannel.

### Added

- Unit-test coverage for the core pure-logic paths: the text processor (spacing / punctuation / capitalization / dev-term and replacement handling), the hotkey combo parser + virtual-key lookup table, and per-provider key resolution. (68 tests, up from 53.)

### Fixed

- Docs: the SECURITY.md vulnerability-disclosure channel no longer has an unfilled email placeholder (now points to a private GitHub Security Advisory); README links the changelog; corrected a stale "not yet live-verified" note on the OpenAI adapter (it's verified).

## [0.1.6] - 2026-07-04

### Changed

- Settings → Speech-to-text provider: **Manage keys… and Test all keys now sit on the dropdown's row** (one row shorter).
- Settings → Dictation: the **Record buttons are gone** — each hotkey field now has a small, subtle record dot tucked into its right edge (click it, then press a key). The two input halves are laid out independently so neither can squeeze the other.
- Settings → Application: the four toggles are now in **two columns**.
- The **Text replacements…** button no longer stretches full-width — it sizes to its label.

## [0.1.5] - 2026-07-04

### Added

- **Enable text replacements** toggle in Settings — a master on/off switch for the whole replacement pass (the saved list is kept, just not applied while off).
- The **Check for updates** flow now shows a spinning arc for at least ~2 seconds before the result lands, so the check reads as actually doing something instead of flashing past.

### Changed

- Settings → Dictation is now laid out as a grid: a 2×2 block of labeled inputs (Language / Mode / Toggle hotkey / Hold hotkey) over two columns of checkboxes.
- All text fields and dropdowns in Settings share one control height, so inputs, dropdowns and buttons line up.
- Removed the redundant "N key(s) configured" line from the provider card (the Manage keys… modal already shows the keys and their status).

## [0.1.4] - 2026-07-04

### Added

- **Record hotkey**: a "Record" button next to each hotkey field in Settings — click it and press a key/combo to set the hotkey.
- **Bulk text-replacements editor**: the Text replacements modal has a "Text editor" toggle that shows all replacements as `from => to` lines, so a big set can be pasted/copied at once.

### Changed

- The tray menu is now minimal (version, Settings…, Open Executable Location, Quit). **About**, **Check for updates** (opens the About window with the live version status), **Open log file**, and **Edit settings.json** moved into the Settings window.
- Fixed the "Save && Restart" button showing a double ampersand — now "Save & Restart".

## [0.1.3] - 2026-07-04

### Added

- Auto-default provider: if the configured `stt_provider` has no keys but another provider does, the app opens straight into that provider (so pasting only, say, Google keys just works). An explicit `--provider` is always respected.
- The settings template is now **baked into the exe** (`include_str!` of settings.example.json); on first run, when no settings.json exists, it's written out from that template — no separate settings.example.json file shipped alongside.
- `scripts/check.ps1`: local CI — runs the exact fmt/clippy/build/test gates GitHub CI runs, so you can verify a change in ~1 minute instead of waiting on GitHub.

### Changed

- Empty-key onboarding notice is now **provider-agnostic** ("No API keys found" instead of naming ElevenLabs) — QuickDictate works with any provider.
- Updated the settings window to **egui/eframe 0.35** (from 0.31); the key/text-replacement modals now use egui's native `Modal`.

## [0.1.2] - 2026-07-04

### Added

- **Settings window** (tray → "Settings…"): provider dropdown, API-key manager in a modal (masked keys, add/remove, per-key status chips, "Test all" probing every key **in parallel** against the real provider API), text-replacements editor modal, hotkey fields with validation, and all the common toggles — styled to the LunarWerx look (brand-blue rounded checkboxes and buttons, carded sections, Segoe UI, dark/light theme). `settings.json` stays the source of truth; "Edit Settings (JSON)" remains in the tray for advanced fields.
- Headless UI screenshots for development: `QUICKDICTATE_UI_SHOT=<png>` makes the settings window capture itself via egui's viewport screenshot (`scripts/ui_shot.ps1` wraps the loop; `-Open keys-test` also runs a live parallel key test before capturing).

## [0.1.1] - 2026-07-04

### Added

- Key prewarm (`prewarm_keys`, default on): the active provider's keys are probed in the background at startup; dead/limited keys are pre-marked and the first validated key is queued ready for the first dictation.
- `--provider <id>` command-line override for `stt_provider`, plus a `QuickDictate-Launcher.bat` menu for launching with any of the six providers.
- Dev-trigger `about` command (opens the About window without the tray).

### Changed

- Key health now lives in memory only — `key-health.json` is gone. Every launch starts fresh and re-probes, so a temporarily limited key or a provider outage never permanently brands a key dead. Failed keys cool down (duration scaled to the failure kind) and become eligible again automatically.
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
