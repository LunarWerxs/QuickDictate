# Provider Setup Guide

QuickDictate supports bring-your-own-key cloud speech-to-text (STT), or an
optional local model that keeps audio offline. This guide covers all seven
providers.

> **As of 2026-07.** Provider endpoints, consoles, model names, and pricing
> pages can and do change after this was written — if a link or field name
> below doesn't match what you see, check the provider's current
> documentation.

## Before you start

1. Copy `settings.example.json` to `settings.json` if you haven't already.
   `settings.json` is gitignored — your keys never get committed.
2. Pick a provider and set `"stt_provider"` to its exact value (listed in each
   section below).
3. Add one or more keys to that provider's key array (also a JSON array of
   strings — you can list multiple keys and QuickDictate will round-robin
   across them with per-key health tracking and cooldown backoff, handy for
   e.g. a personal key plus a work key).
4. Save and restart QuickDictate for the change to take effect.

Only the key array for your active cloud `stt_provider` needs to be filled in;
the others can stay empty. The Local provider needs no key.

---

## ElevenLabs

- **`stt_provider` value:** `"elevenlabs"`
- **Key array:** `"elevenlabs_keys"`
- **Engine:** ElevenLabs Scribe v2 realtime (streaming)

**Getting a key:**
1. Go to <https://elevenlabs.io/app/settings/api-keys>.
2. Sign up or log in.
3. Create an API key on that page and copy it.
4. Paste it into `elevenlabs_keys` in `settings.json`, e.g. `["your-key-here"]`.

**Notes:** Streaming provider — you'll see words appear live as you speak.

**Pricing:** Check ElevenLabs' official pricing page for current rates; prices
and included quotas drift over time, so don't rely on cached numbers.

---

## Deepgram

- **`stt_provider` value:** `"deepgram"`
- **Key array:** `"deepgram_keys"`
- **Engine:** Deepgram nova-3 (streaming)

**Getting a key:**
1. Go to <https://console.deepgram.com/signup>.
2. Create an account (Deepgram typically offers free credit on signup — check
   the console for current terms).
3. In the Deepgram console, create an API key.
4. Paste it into `deepgram_keys` in `settings.json`.

**Notes:** Streaming provider — live word count as you speak.

**Pricing:** Check Deepgram's official pricing page for current rates; figures
change over time.

---

## OpenAI

- **`stt_provider` value:** `"openai"`
- **Key array:** `"openai_keys"`
- **Engine:** `gpt-4o-transcribe` via OpenAI's GA Realtime API, streaming at 24 kHz

**Getting a key:**
1. Go to <https://platform.openai.com/api-keys>.
2. Sign up or log in to your OpenAI platform account.
3. Create a new secret key and copy it (you won't be able to view it again
   later).
4. Paste it into `openai_keys` in `settings.json`.

**Notes:** Uses the general-availability Realtime API (not a beta/preview
endpoint), streaming audio at 24 kHz. Streaming provider — live word count.

**Pricing:** Check OpenAI's official pricing page for current Realtime API
rates; pricing drifts and varies by model.

---

## AssemblyAI

- **`stt_provider` value:** `"assemblyai"`
- **Key array:** `"assemblyai_keys"`
- **Engine:** AssemblyAI Universal-Streaming (v3), streaming, English

**Getting a key:**
1. Go to <https://www.assemblyai.com/dashboard/signup>.
2. Sign up or log in.
3. Copy your API key from the dashboard.
4. Paste it into `assemblyai_keys` in `settings.json`.

**Notes:** Streaming provider (live word count). Universal-Streaming v3 in
QuickDictate is English-only.

**Pricing:** Check AssemblyAI's official pricing page for current rates;
figures drift.

---

## DashScope (Alibaba Cloud)

- **`stt_provider` value:** `"dashscope"`
- **Key array:** `"dashscope_keys"`
- **Engine:** Alibaba Cloud DashScope Paraformer (`paraformer-realtime-v2`), streaming

**Getting a key:**
1. Go to <https://dashscope.console.aliyun.com/apiKey>.
2. Sign up or log in to Alibaba Cloud and open the DashScope console.
3. Create an API key and copy it.
4. Paste it into `dashscope_keys` in `settings.json`.

**Notes — region matters:** DashScope keys are region-locked.
- By default QuickDictate connects to the **mainland-China** host
  (`"dashscope_intl": false`).
- If your key was issued for the **International** region, set
  `"dashscope_intl": true` in `settings.json`.
- A key from the wrong region will simply fail to connect — if DashScope
  doesn't work, this mismatch is the first thing to check.

Streaming provider — live word count.

**Pricing:** Check Alibaba Cloud DashScope's official pricing page for current
rates; figures drift and can differ between the mainland and international
offerings.

---

## Google Cloud Speech-to-Text

- **`stt_provider` value:** `"google"`
- **Key array:** `"google_keys"`
- **Engine:** Google Cloud Speech-to-Text, **batch** v1 (`speech:recognize` REST endpoint, plain API key via `?key=`)

**This provider is different from the other five:** it uses batch recognition,
not a live stream. There is **no live word count while you're speaking**.
QuickDictate uploads completed 55-second blocks in the background during an
unusually long dictation so audio memory stays bounded, then sends the final
partial block when you stop. All results remain withheld until release and
arrive together, in order.

**Getting a key:**
1. Go to <https://console.cloud.google.com/apis/credentials>.
2. Create or select a Google Cloud project.
3. Enable the **"Cloud Speech-to-Text API"** for that project.
4. Attach a billing account to the project. Google's free tier for this API is
   typically around 60 minutes/month, but confirm current limits in the
   console — this is exactly the kind of figure that drifts.
5. Create an API key under **Credentials** and copy it.
6. Paste it into `google_keys` in `settings.json`.

**Model note:** QuickDictate talks to the v1 endpoint and only supports v1
models (`latest_long` / `default`). Newer v2/Chirp models reject plain API-key
auth, so they are out of scope here — don't set `stt_model` to a v2/Chirp
model name for this provider.

**Pricing:** Check Google Cloud's official Speech-to-Text pricing page for
current rates and free-tier limits; both drift over time.

---

## Local (offline)

- **`stt_provider` value:** `"local"`
- **Key array:** none
- **Mode:** offline batch; the transcript arrives after hotkey release
- **Runtime:** pinned `transcribe.cpp` 0.1.3 CPU/Vulkan package

Choose **Local (offline)** in Settings. Pick any model, click **Install**, wait
for the verified download, then Save. You can install one model or all three:

| `local_model` | Model | Download | Intended tradeoff |
|---|---|---:|---|
| `cohere-bf16` | Cohere Transcribe 03-2026 BF16 | 3.82 GiB | Highest numeric fidelity |
| `cohere-q5` | Cohere Transcribe 03-2026 Q5_K_M | 1.65 GiB | Default; near-lossless accuracy/size balance |
| `whisper-turbo-q5` | Whisper Large v3 Turbo Q5_K_M | 591 MiB | Smallest and broadest language coverage |

The executable contains none of these weights. Downloads go under
`%LOCALAPPDATA%\QuickDictate\local-stt`; a shared runtime adds roughly 80 MiB
once. Every artifact is pinned to an immutable upstream revision and verified
by exact byte count plus SHA-256 before an atomic rename makes it usable.
Interrupted downloads remain `.part` files and are discarded on the next
attempt. **Remove** deletes that model's directory; the small shared runtime
stays available for other models.

The model remains loaded briefly between dictations for speed, switches
automatically when you select another model, unloads when you switch away from
Local, and unloads after five idle minutes. Vulkan is preferred when available;
CPU is the automatic fallback. Raw audio passes directly from QuickDictate's
16 kHz pipeline to the native runtime—there is no temporary WAV file or
Python/PyTorch environment.

Local packs come from:

- [handy-computer/transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) (MIT)
- [Cohere Transcribe GGUF](https://huggingface.co/handy-computer/cohere-transcribe-03-2026-gguf) (Apache-2.0 model)
- [Whisper Large v3 Turbo GGUF](https://huggingface.co/handy-computer/whisper-large-v3-turbo-gguf) (MIT model)

---

## Choosing a provider

ElevenLabs, Deepgram, OpenAI, AssemblyAI, and DashScope are all **streaming**
providers: audio goes out over a WebSocket as you talk and you see your words
appear live, word by word. **Google** is a cloud **batch**
(bounded HTTPS segments), so you speak, then pause, and the whole transcript
for that utterance arrives at once with no live word count in between. If you
want the most immediate, "watch it type as I talk" feel, pick one of the five
streaming providers; pick Google only if you specifically want Google's
recognition quality/language coverage and can live without live word-by-word
feedback. Pick **Local** when privacy/offline use matters most and your machine
has enough RAM/disk for the selected model; it also returns results on release.

Whichever you choose, remember: your audio and API keys go only to the
provider you select, never to the QuickDictate maintainer. (The only thing the
app ever reports is the anonymous daily update-check ping described in
[SECURITY.md](../.github/SECURITY.md).)
