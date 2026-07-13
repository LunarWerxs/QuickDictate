# Security Policy

## Supported Versions

QuickDictate is under active development, and security fixes are made against the **latest release only** (currently the `0.4.x` line) — please update to the newest release before reporting an issue.

| Version | Supported |
| ------- | --------- |
| 0.4.x (latest release) | Yes |
| Older releases | No |

## Reporting a Vulnerability

QuickDictate is a small open-source project without a dedicated security team, but reports are taken seriously.

Please **do not** open a public GitHub issue containing exploit details, proof-of-concept code, or other specifics that could put existing users at risk before a fix ships.

Instead, open a [private GitHub Security Advisory](../../security/advisories/new) on this repository. This lets you share details privately with the maintainer and coordinate a fix and disclosure timeline before anything is made public.

Please include:
- The QuickDictate version (and whether you built from source or used a release binary).
- Which STT provider you were using, if relevant.
- Steps to reproduce, or a minimal example.

You should receive an acknowledgment as soon as reasonably possible. Once a fix is available, details can be disclosed publicly and credited to the reporter, if desired.

## Where Your Secrets Live

QuickDictate is **bring-your-own-key**: you supply your own API key(s) for the STT provider(s) you use.

- API keys are stored **only** in your local `settings.json`, which is excluded from version control via `.gitignore`. It never leaves your machine as part of the project's source or build artifacts.
- The repository ships a `settings.example.json` template with empty key arrays — no real credentials are ever committed.
- The compiled `quickdictate.exe` contains **no embedded credentials**. Every build, whether downloaded or built from source, starts with no keys until you add your own.
- Multiple keys per provider are supported and are round-robined locally with per-key health tracking (alive / quota / dead) and cooldown backoff. This local rotation logic never transmits your keys anywhere except directly to the provider's API endpoint you configured.
- The QuickDictate maintainer never sees, receives, or has access to your API keys.

## What Data Leaves Your Machine

- When you dictate, your **microphone audio is streamed directly to the third-party cloud speech-to-text provider you configure** in `settings.json` (one of: ElevenLabs, Deepgram, OpenAI, AssemblyAI, DashScope, or Google Cloud Speech-to-Text) — over WebSocket for the streaming providers, or HTTPS for the Google batch provider. Audio goes only to that provider, using your own API key.
- **The update check goes to LunarWerx's own endpoint.** With *Check for updates daily* on (`update_auto_check`, the default; the check runs at most once per 24 hours), QuickDictate asks `https://studio.connections.icu/v1/app/quickdictate/latest` whether a newer release exists. The endpoint relays GitHub's release info verbatim. Turning the toggle off stops the daily check entirely; the manual "Check for Updates…" button sends the same request, but only when you click it. The release binary itself still downloads directly from GitHub.
- Beyond the update check described above, your audio, transcripts, and API keys never leave your machine except to the STT provider you configured, as described above.
- **One opt-in exception: Connections settings sync.** QuickDictate can optionally sync your *portable preferences* (mode, language, hotkeys, STT provider/model, and similar) to a LunarWerx Connections account, so they follow you between machines. This is **off by default** and only activates if you explicitly sign in from the Settings window. When enabled, it syncs preferences only — it never transmits your API keys, audio, or transcripts. See [docs/SETTINGS_SYNC.md](../docs/SETTINGS_SYNC.md) for exactly what syncs and how to turn it off.
- **Local logging is summary-only by default.** When `enable_logging` (or `QUICKDICTATE_LOG`) is on, `quickdictate.log` records event summaries — char counts, provider, timing — never your recognized text. The separate `log_transcripts` setting (off by default) opts into writing your full dictated text to that local log file; only enable it for deep debugging, and turn it back off afterwards. Either way, this stays a local file on your machine — it is never transmitted anywhere.
- Recognized text is delivered locally to whatever application currently has focus, via synthesized keystrokes — it does not pass through any additional network service beyond the STT provider itself.
- By choosing a provider, you are trusting **that provider's** privacy policy and data-handling practices for the audio and resulting transcripts sent to them. Review the provider's own terms before use, since QuickDictate has no control over how they process or retain your data.

## Antivirus / SmartScreen Note

QuickDictate installs a global hotkey listener and synthesizes keystrokes to paste transcribed text into the focused window. This is functionally similar to what a keylogger does, so:

- **Windows Defender or other antivirus software may flag the app.** This is a known false positive caused by the keystroke-injection technique itself, not malicious behavior. The source is available in this repository for inspection.
- **The released `.exe` is currently unsigned**, so Windows SmartScreen will likely show a "Windows protected your PC" warning on first run. To proceed, click **More info**, then **Run anyway**.
- If you'd rather not trust an unsigned binary, you can build QuickDictate yourself from source (see the README) and verify the code you're running.
