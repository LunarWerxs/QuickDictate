# QuickDictate

> Hold a hotkey, speak, and it types into any focused Windows app - via your own STT key or fully offline.

<!-- odin:about HAND-OWNED above the GENERATED marker. Edit freely; `odin codex about --ingest` carries it back into Odin's Codex. -->

## What it is

QuickDictate is a small Windows tray app for voice dictation: hold or tap a global hotkey, speak, and the transcript types straight into whatever window has focus (editor, chat box, browser field, terminal). It talks to whichever cloud speech-to-text provider the user brings their own API key for (ElevenLabs, Deepgram, OpenAI, AssemblyAI, DashScope, or Google), or runs a fully offline local model (Cohere Transcribe or Whisper Large v3 Turbo) with no key, account, or internet needed. There is no QuickDictate account or subscription; an optional sign-in only enables settings sync and an LLM cleanup pass, both opt-in. Licensed PolyForm Noncommercial 1.0.0 from v0.9.0 on (source available, free for personal and other noncommercial use, commercial use by separate license); releases through v0.8.0 were MIT.

## Things not to forget

_The intricacies worth remembering: the gotchas, the half-built parts, the decisions whose
reason lives nowhere else. Odin never overwrites this section._

- Settings sync deliberately never carries API keys, audio, or transcripts across devices - only portable preferences like hotkeys and language travel, and a compiled-in test partitions every Config field into SYNCED_KEYS or NEVER_SYNCED so a newly added setting can never silently fall through uncategorized. anchors: `src/sync/schema.rs:42`
- Dictation history (capped at 50 entries) is written to `quickdictate-history.json` in the data folder after every dictation, so it survives restarts and self-updates; `persist_history` (default on, Advanced page) turns that off and deletes the file. The file is dictated text and is never synced or reported - only the boolean preference travels. Until v0.9.0 it was memory-only, and an update wiped the day's dictations. anchors: `src/history_store.rs:1`, `src/state.rs:17`
- Pause-gated punctuation voice commands (period, comma, new line) were deliberately deferred and never built because naive matching on a streamed transcript is unsafe - only the single 'scratch that' command shipped. anchors: `src/voice_commands.rs:1`
- The optional LLM polish pass verifies every edit is a verbatim, non-overlapping substitution before applying it, rejecting any reply that isn't - a guard against the model inventing or rewording content instead of just cleaning it up. anchors: `src/polish/edits.rs:23`
- API keys and sync credentials are sealed with Windows DPAPI before ever touching disk, so the plaintext key exists only in memory for as long as the process runs. anchors: `src/secretstore.rs:69`
- Offline transcription is not bundled - it needs a separate on-demand model download (Cohere Transcribe ~1.65GiB or Whisper Large v3 Turbo ~591MiB) that is size- and SHA-256-verified before install. anchors: `src/local_stt/mod.rs:40`
- The opt-in anonymized daily usage report to LunarWerx is a distinct toggle from Connections settings sync: one sends a single anonymous aggregate rollup keyed by the same install id the update checker uses, the other syncs a signed-in user's own settings and stats back to their own account - neither carries transcript text. anchors: `src/stats/report.rs:43`
- Custom vocabulary and text replacements are applied before the transcript is typed, and per-app profiles can override provider, language, vocabulary, and formatting by matching the focused window's exe name - so the same phrase can come out differently in a terminal versus an email client. anchors: `src/config/query.rs:80`
- The key pool's rules exist because of one real lockout (2026-09-10): a transient failure benches a key for two seconds, never thirty; a merely-cooling key is still offered rather than failing the press; a connect timeout is the network's fault and benches nothing; and one press never tries the same key twice (`acquire_excluding` with the press's `tried` list). Loosen any of these and a single stalled handshake on the only good key locks dictation out again. anchors: `src/keys.rs:56`, `src/stt/mod.rs:161`
- Licensed PolyForm Noncommercial 1.0.0 from v0.9.0 on; releases through v0.8.0 were MIT and that grant stands for those copies (LICENSE preamble). The About box, the exe's version block, and the README badge all state the license, so a future change touches all four. anchors: `LICENSE:1`, `src/about/layout.rs:99`

<!-- odin:about GENERATED BEGIN - rewritten by `odin codex about --publish`; edit the Codex, not this -->

## What Odin knows about this project

Everything from here down is generated from this project's Codex dossier
(`codex/projects/quickdictate-beta1.md` in the Odin clone) and is **rewritten on every publish** -
edit the dossier, not this block. Everything ABOVE the marker is yours.

### At a glance

- **Ships as:** tray app (Windows 10/11 x64), delivered as a portable .exe GitHub release with an in-app silent auto-updater; also buildable from source via cargo
- **Live at:** https://quickdictate.github.io/
- **Written in:** Rust (144 files), PowerShell (10 files)
- **Built with:** Serde, Tokio, cpal, egui, image, libloading, reqwest, tracing, windows-rs
- **Package:** `quickdictate` 0.9.0
- **Entry points:** `cargo_bins`
- **CI:** `ci.yml`, `release.yml`
- **Domain:** dictation, speech-to-text, voice-typing, hotkeys, system-tray, text-injection
- **Remote:** https://github.com/LunarWerxs/QuickDictate.git

### Architecture

- `src/main.rs` - Entry point only now: logging setup, single-instance guard, wires threads together, main event loop; startup sequencing and the hotkey/session loop it used to hold were split out (see src/startup.rs, src/session_loop.rs)
- `src/stt/` - Provider-agnostic STT session runner (mod.rs) plus one adapter per cloud provider and a local adapter, behind the SttProvider trait in provider.rs
- `src/settings_ui/` - The egui-based Settings window: cards, logic/ (now a directory: draft/hotkey/keytest/save/screenshot/sync_ops), modals, nav rail, widgets, styling
- `src/local_stt/` - Offline model lifecycle: download/verify/install the Cohere or Whisper runtime and weights (mod.rs), load them via a native FFI engine (native.rs), transcribe
- `src/keys.rs` - In-memory pool of the user's API keys per provider with health tracking, cooldown, and automatic rotation on failure
- `src/output/` - Hybrid text injection into the focused window: worker.rs (session), input.rs (Unicode keystrokes), clipboard.rs (paste-and-restore)
- `src/hotkeys/, src/mouse_hook/` - Global keyboard and mouse-button hotkey registration/hooking for hold-to-talk and tap-to-toggle
- `src/config/` - settings.json schema (schema.rs): global settings, per-app Profile overrides; query.rs resolves effective settings; store.rs handles load/save/key sealing
- `src/sync/` - Optional OAuth (PKCE, oauth.rs) sign-in to LunarWerx Connections and push/pull of a settings+stats document (mod.rs); never syncs keys, audio, or transcripts
- `src/polish/` - Optional LLM cleanup pass over a transcript (polisher.rs) on a strict latency budget, verifying edits are verbatim substitutions before applying (edits.rs)
- `src/stats/` - Persistent, privacy-safe usage statistics (usage.rs) with atomic save and multi-device merge (store.rs)
- `src/ui/, src/about/, src/theme.rs` - Tray icon (ui/tray.rs), cursor-following status pip (ui/overlay.rs), About box (about/), dark-mode-aware palette (theme.rs)
- `src/paths/, src/autostart.rs, src/secretstore.rs` - Data-directory resolution/relocation (paths/resolve.rs, paths/migrate.rs), Windows Run-key autostart, DPAPI seal/unseal for secrets at rest
- `src/update/` - Checks GitHub releases (mod.rs), verifies asset hash/signature, downloads and silently swaps the running portable exe (install.rs, flows.rs)
- `src/nudge.rs, src/nudge_engine.rs` - Sign-in nudge decision engine: cadence/ladder logic for when to show a soft prompt to connect a Connections account
- `src/logging/` - Undocumented in the original entry: log file rotation/writer (SizeCappedLogWriter likely lives here now, not in main.rs as data_stores claims)
- `src/startup.rs, src/session_loop.rs` - Undocumented in the original entry: startup.rs is single-instance hand-off + settings/logging/audio bring-up (bring_up_app); session_loop.rs is the hotkey/session event loop, both split out of main.rs
- `scripts/` - PowerShell build/release helper scripts
- `docs/` - User guide, provider setup notes, settings-window and settings-sync reference, release process
- `.github/` - CI (ci.yml) and release (release.yml) GitHub Actions workflows

### Features

27 recorded - 27 shipped, 0 partial, 0 planned. Each `path:line` is where the feature is DEFINED, checked by `odin codex check`.

**Shipped**

- **Global hotkey dictation (hold or tap, keyboard or mouse button)** - Hold a key to talk or tap to toggle, with a configurable global keyboard or mouse-button binding, while microphone audio is captured continuously in the background. - `src/hotkeys/register.rs:102`, `src/mouse_hook/callback.rs:206`, `src/audio/capture.rs:126`
- **Bring-your-own-key cloud speech-to-text (6 providers)** - User pastes their own API key for ElevenLabs, Deepgram, OpenAI, AssemblyAI, DashScope, or Google and QuickDictate streams (or batches, for Google) audio to that provider directly; no QuickDictate account or fee involved. - `src/stt/provider.rs:104`, `src/stt/mod.rs:99`, `src/stt/elevenlabs.rs:44`
- **Fully offline local transcription** - Install a Cohere Transcribe or Whisper Large v3 Turbo model (downloaded, size- and SHA-256-verified on demand) and dictate with no API key, account, or network connection; audio never leaves the PC. - `src/local_stt/mod.rs:30`, `src/local_stt/native.rs:87`, `src/stt/local.rs:19`
- **Types the transcript into whatever window has focus** - Injects the recognized text into the focused app via simulated Unicode keystrokes for short bursts or clipboard copy-paste-restore for long text, working in any editor, browser, chat app, or terminal. - `src/output/worker.rs:30`, `src/output/input.rs:50`, `src/output/clipboard.rs:25`
- **Custom vocabulary** - A user-maintained word list, edited on its own Vocabulary page in Settings, is sent to the active provider as recognition hints (or biasing terms) so jargon and names are transcribed correctly, and can be overridden per profile. - `src/config/schema.rs:402`, `src/stt/provider.rs:74`, `src/settings_ui/vocabulary.rs:13`
- **Text replacements and auto-formatting** - A user fix-list of case-insensitive word/phrase replacements plus automatic capitalization, punctuation cleanup, spacing, and filler-word removal are applied to every transcript before it is typed. - `src/text.rs:69`, `src/text.rs:186`, `src/text.rs:220`
- **Per-app profiles** - Rules matched against the focused application's exe name can override the provider, language, vocabulary, and formatting toggles, so e.g. a terminal and an email client dictate differently. - `src/config/schema.rs:31`, `src/focus.rs:22`, `src/config/query.rs:80`
- **Searchable dictation history** - The last 50 dictations are browsable and filterable in Settings on a list that fills its page; rows can be ticked (the box or the text) and copied together oldest-first with a blank line between, or copied / pasted again singly from icon buttons, and entries carry stable ids so ticks survive new dictations. Since v0.9.1 the list is written to quickdictate-history.json in the data folder after every dictation and reloaded at startup, so it survives a restart and a self-update; persist_history (default on, Advanced page) turns that off and deletes the file. The file is dictated text, so it is local only - never synced, never in the usage rollup or an error report. - `src/history_store.rs:29`, `src/state.rs:309`, `src/settings_ui/history.rs:87`
- **"Scratch that" voice command** - Saying the phrase during dictation discards the just-typed text instead of inserting it, an opt-in precision-limited voice command. - `src/voice_commands.rs:32`, `src/output/worker.rs:210`
- **Settings window** - A single egui window with a left rail of five pages (Application / Dictation / Vocabulary / History / Advanced): provider and key setup, the everyday behaviour toggles and settings sync; hotkeys, timing and formatting; the custom vocabulary editor; the transcript history; and the set-and-forget switches (key prewarm, tray icon, logging, error reports, usage stats, per-app profiles, history persistence, data folder). Opened from the tray or on first run. An available update shows as a banner above the page whose Update button opens About and starts the install at once (about::show_about_and_install), rather than only opening About and leaving the pill to be found. - `src/settings_ui/mod.rs:178`, `src/settings_ui/nav.rs:24`, `src/settings_ui/banners.rs:58`
- **API key pool with health tracking and failover** - Multiple keys per provider; the pool tracks per-key status (untested/alive/quota/dead) and benches a rejected key for six hours and a rate-limited one for a minute, but a transient failure for only two seconds. A merely-cooling key is still offered, so one blip never locks the user out of their only working key; a connect timeout is the network's fault and benches nothing; one press never tries the same key twice (the retry shell passes the keys it has already tried); and the failure cause is named on the pip. Per-app-profile provider pools are cached on App so their health survives across presses. - `src/keys.rs:140`, `src/keys.rs:286`, `src/stt/mod.rs:186`
- **Optional LLM polish pass** - An opt-in cleanup round-trip to an LLM endpoint edits the raw transcript for clarity within a strict latency deadline, rejecting any reply that isn't a verbatim, non-overlapping edit set. - `src/polish/polisher.rs:12`, `src/polish/edits.rs:23`, `src/config/schema.rs:452`
- **Optional settings sync via Connections sign-in** - Signing in with a LunarWerx Connections account (OAuth/PKCE) syncs preferences like hotkeys and language across machines and merges usage stats; API keys, audio, and transcripts are never synced. - `src/sync/oauth.rs:151`, `src/sync/mod.rs:118`, `src/settings_ui/sync.rs:10`
- **Silent self-update** - Checks the LunarWerx releases endpoint (GitHub as fallback) on a daily timer, verifies the new executable's hash, and downloads and swaps the running portable exe in place with the app relaunching itself; it asks before installing unless update_auto_install is on. Since v0.9.1 the relaunch restores the windows that were open rather than only About: update::install::Reopen records about::is_open() / settings_ui::is_open() and passes --show-about and/or --relaunch to the new process. - `src/update/install.rs:228`, `src/update/install.rs:258`, `src/update/flows.rs:143`
- **Usage statistics** - Tracks words, audio time, and dictation counts overall and per provider, shown as charts/tiles in Settings, with a reset option; no transcript content is stored. - `src/stats/usage.rs:13`, `src/stats/store.rs:144`, `src/settings_ui/widgets.rs:352`
- **Tray icon with live status pip** - A tray icon with a menu (history, settings, quit) plus a small overlay pip that follows the mouse cursor to show listening/processing/error state and a live word count. - `src/ui/mod.rs:109`, `src/ui/tray.rs:236`, `src/ui/overlay.rs:37`
- **Sign-in nudge** - A soft, capped-frequency in-app banner invites the user to sign in to Connections after enough usage, following an age/session-gated cadence engine that never asks for money and retires itself once signed in. - `src/nudge_engine.rs:278`, `src/settings_ui/banners.rs:175`, `src/nudge.rs:287`
- **Secrets sealed at rest** - API keys and sync credentials are sealed with Windows DPAPI before being written to settings.json/disk, so the plaintext key exists only in memory. - `src/secretstore.rs:69`, `src/secretstore.rs:84`, `src/config/store.rs:222`
- **Start with Windows** - A toggle registers/removes QuickDictate from the per-user Windows Run key so it launches automatically at sign-in. - `src/autostart.rs:37`, `src/config/schema.rs:266`
- **Relocatable data directory** - Logs, stats, and update cache default to next to the exe but can be moved to %LOCALAPPDATA% or a chosen folder from Settings, migrating existing files automatically. - `src/paths/resolve.rs:13`, `src/paths/migrate.rs:65`, `src/settings_ui/application.rs:108`
- **About box with update check** - A native About window shows the current version, license, and a status pill that checks for and can trigger an update install. - `src/about/mod.rs:131`, `src/about/updates.rs:19`, `src/about/updates.rs:102`
- **Dark-mode-aware theming** - The Settings window, tray overlay, and native About box follow Windows' light/dark app theme. - `src/theme.rs:121`, `src/theme.rs:169`
- **Choose which microphone to dictate from** - An input_device setting pins dictation to one microphone by a case-insensitive name substring instead of always following the Windows default recording device, falls back to the default when the named device is absent, and the running capture now follows a live microphone change without needing a restart. - `src/config/schema.rs:183`, `src/audio/capture.rs:34`, `src/settings_ui/logic/save.rs:26`
- **Occasional feedback survey prompt** - A cadence-gated "how's it going?" prompt appears after enough usage (14 days installed, 5 dictation sessions) and at most once a quarter thereafter, reusing the sign-in nudge's age/session-gate-plus-cooldown pattern in its own independently persisted state machine; answering opens a pre-filled GitHub issue in the browser since there is no backend to collect free text, and declining costs nothing. - `src/feedback_survey.rs:278`, `src/feedback_survey.rs:66`, `src/feedback_survey.rs:56`
- **Opt-in local error reports** - An off-by-default "Enable local error reports" toggle reveals a Settings action that assembles the version, active provider, and recent quickdictate.log/quickdictate-panic.log lines (redacted of anything resembling a key or token) into an editable preview the user can save as a timestamped .txt file to attach to a GitHub issue themselves; nothing here ever makes a network call. - `src/error_report.rs:103`, `src/error_report.rs:160`, `src/settings_ui/advanced.rs:371`
- **Opt-in anonymized usage report to LunarWerx** - A distinct, off-by-default "share usage stats" toggle sends LunarWerx one anonymized daily aggregate rollup (provider mix, word/audio/dictation totals, keyed by the same anonymous install id the update checker uses) to a studio.connections.icu endpoint; no transcript text, hostname, username, or account identity is included, and this is separate from Connections settings sync, which carries a signed-in user's own stats back to their own account. - `src/stats/report.rs:43`, `src/stats/report.rs:145`, `src/settings_ui/advanced.rs:180`
- **Crash-detection banner** - On launch, if the previous run left a fresh quickdictate-panic.log entry, a dismissible non-modal banner in Settings offers to open the same redacted crash-report preview or dismiss it; stays entirely off unless local error reports are enabled, and the panic hook now also records for this feature when error reporting alone is on. - `src/crash_banner.rs:64`, `src/settings_ui/banners.rs:90`, `src/startup.rs:147`

### Where to add a new one

- **a new cloud STT provider** - implement the SttProvider/ProviderSink/ProviderStream traits in a new src/stt/<name>.rs, then register it in make_provider anchors: `src/stt/provider.rs:104`, `src/stt/mod.rs:99`
- **a new offline local model** - add a ModelSpec entry to the MODELS constant and wire any backend-specific handling into NativeEngine anchors: `src/local_stt/mod.rs:40`, `src/local_stt/native.rs:92`
- **a new setting** - add a field (with a default_ fn) to Config or Profile in config.rs, then surface it in a settings_ui card and, if user-facing, add it to sync.rs's SYNCED_KEYS or NEVER_SYNCED anchors: `src/config/schema.rs:111`, `src/settings_ui/cards.rs:109`, `src/sync/schema.rs:42`
- **a new Settings tab/page** - add a variant to the Tab enum in nav.rs, add its render branch in SettingsApp::ui, and put its widgets in a new or existing cards.rs section anchors: `src/settings_ui/nav.rs:23`, `src/settings_ui/mod.rs:164`
- **a new hotkey-bindable action** - extend HotkeyBindings/HotkeyEvent in hotkeys.rs and add the dispatch arm in main.rs's handle_hotkey_event anchors: `src/hotkeys/mod.rs:112`, `src/session_loop.rs:109`
- **a new text-replacement rule or voice command** - static replacements/regex live in TextProcessor (text.rs); a new spoken command follows the ScratchThat pattern in voice_commands.rs and is wired into output.rs's transcript handling anchors: `src/text.rs:69`, `src/voice_commands.rs:13`

### Gaps and wants

_Withheld: this repository is public, and the gap list is not published outside the private index._
_Read it with `python odin.py codex brief quickdictate-beta1` in the Odin clone._

---

_Generated by `odin codex about --publish quickdictate-beta1` on 2026-09-10 from a Codex dossier stamped 2026-09-10. Regenerate after the product moves; `odin codex about` reports drift._
<!-- odin:about GENERATED END sha=319762afa651 -->
