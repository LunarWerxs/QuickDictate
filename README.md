<div align="center">

<a href="https://quickdictate.github.io/">
  <img src="assets/og-image.png" alt="QuickDictate — talk instead of type, in any app" width="820">
</a>

<h1>QuickDictate</h1>

<p><b>Press a key, talk, and your words land wherever your cursor already is.</b></p>

<p>
A tiny Windows tray app for voice dictation. Hold or tap a global hotkey, speak, and the
transcript types straight into whatever window has focus — your editor, a chat box, an email,
a terminal, any web text field. Use <i>your own</i> speech-to-text API key, or install an
optional local model for fully offline transcription — <b>no QuickDictate subscription or account</b>.
</p>

<p>
  <a href="https://quickdictate.github.io/"><b>🌐 Website</b></a>
  &nbsp;·&nbsp;
  <a href="https://github.com/LunarWerxs/QuickDictate/releases/latest"><b>⬇️ Download</b></a>
  &nbsp;·&nbsp;
  <a href="docs/GUIDE.md">📖 Full guide</a>
</p>

<p>
  <a href="https://github.com/LunarWerxs/QuickDictate/releases/latest"><img src="https://img.shields.io/github/v/release/LunarWerxs/QuickDictate?label=release&color=2e7df6" alt="Latest release"></a>
  <a href="https://github.com/LunarWerxs/QuickDictate/releases/latest"><img src="https://img.shields.io/github/downloads/LunarWerxs/QuickDictate/total?color=2e7df6&label=downloads" alt="Downloads"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0078D6" alt="Windows 10/11 x64">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license">
  <a href="https://github.com/LunarWerxs/QuickDictate/stargazers"><img src="https://img.shields.io/github/stars/LunarWerxs/QuickDictate?color=f6b02e" alt="Stars"></a>
</p>

</div>

<br>

<div align="center">
  <img src="docs/images/settings.png" alt="The QuickDictate settings window" width="480">
  <br>
  <sub><i>Everything lives in one small settings window — providers, keys, hotkeys, and toggles.</i></sub>
</div>

## 🆕 New in v0.5.0: fully offline dictation

Choose **Local (offline)** in Settings and QuickDictate can transcribe without an API
key or an internet connection. Microphone audio stays on your PC.

| Model | Download | Best fit |
| :-- | --: | :-- |
| **Cohere Transcribe 03-2026 Q5_K_M** | 1.65 GiB | Accuracy-first default |
| **Whisper Large v3 Turbo Q5_K_M** | 591 MiB | Smaller install and broader language coverage |

- **Manage everything in Settings:** install, select, cancel a download, or delete
  either model without hunting through folders.
- **Small app, on-demand models:** weights are not bundled in the executable or
  repository. They download to `%LOCALAPPDATA%\QuickDictate\local-stt`, use up to
  eight parallel connections when supported, and are size- and SHA-256-verified
  before use. The first install also adds a shared runtime of roughly 80 MiB.
- **Less waiting after setup:** the selected local model prewarms in the background
  and stays ready between dictations. QuickDictate shows a spinner during final
  local processing, queues an early next hotkey press, switches models automatically,
  and releases the model's RAM/VRAM when you return to a cloud provider.
- **Lighter long sessions:** audio, logging, clipboard, update, and network buffers
  are bounded; long Google recordings upload in ordered chunks; idle polling is
  reduced; and v0.5 fixes duplicate audio, non-BMP characters such as emoji, cold
  local results, and **Save & Restart** now returning to Settings.

## ✨ What you get

| | |
| :-- | :-- |
| 🔑 **Cloud or fully local** | Six bring-your-own-key services plus two optional offline models. Switch whenever you like. |
| ⌨️ **Types into any window** | Whatever has focus — your editor, a chat box, a terminal, or a web form. |
| ✋ **Hold or tap** | Hold a key while you talk, or tap to start and stop. Both are configurable. |
| 💬 **Clear live feedback** | Five cloud providers stream words as you talk; batch and Local modes show when the final result is processing. |
| 🪄 **Little touches that add up** | A fix-list for words it mishears, per-app profiles, and a *"scratch that"* voice command. |
| 🔒 **Your data stays yours** | Cloud audio goes only to the provider you pick; Local audio never leaves the PC. Optional settings sync is opt-in. |

## 🚀 Quick start

1. Grab the **[latest release](https://github.com/LunarWerxs/QuickDictate/releases/latest)** (or [build from source](docs/GUIDE.md#build-from-source)).
2. Run `quickdictate.exe`. With no provider configured, Settings opens for you.
3. Pick how you want to transcribe:
   - **Cloud:** choose one of the six services and use **Manage keys…** to paste your API key.
   - **Offline:** choose **Local (offline)**, select Cohere or Whisper, and click **Install**.
4. Click **Save**, then press **F13** to hold or **F14** to toggle and start talking.

> [!TIP]
> Prefer files to forms? QuickDictate still keeps one readable `settings.json` next
> to the executable. Start from `settings.example.json` or edit the generated file.

## 📚 Learn more

Every setting, per-provider setup, and the privacy details live in the
**[complete guide](docs/GUIDE.md)** — with provider-specific notes in
**[docs/providers.md](docs/providers.md)**, including local model and storage details.

## 📄 License

MIT — see [LICENSE](LICENSE). Made with care by **[LunarWerx Studios](https://lunarwerx.com)**.
