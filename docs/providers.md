# Provider Setup Guide

QuickDictate is bring-your-own-key: you supply your own API key(s) for whichever
cloud speech-to-text (STT) provider you want to use, and QuickDictate streams
your microphone audio to that provider to get a transcript back. This guide
walks through getting a key and configuring each of the six supported
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

Only the key array for your active `stt_provider` needs to be filled in; the
others can stay empty.

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

**This provider is different from the other five:** it's record-then-send
batch recognition, not a live stream. There is **no live word count while
you're speaking** — QuickDictate records your utterance, sends it once you
stop, and the transcript comes back roughly a request at a time (each request
tops out around 60 seconds of audio).

**Getting a key:**
1. Go to <https://console.cloud.google.com/apis/credentials>.
2. Create or select a Google Cloud project.
3. Enable the **"Cloud Speech-to-Text API"** for that project.
4. Attach a billing account to the project. Google's free tier for this API is
   typically around 60 minutes/month, but confirm current limits in the
   console — this is exactly the kind of figure that drifts.
5. Create an API key under **Credentials** and copy it.
6. Paste it into `google_keys` in `settings.json`.

**Build requirement:** the Google provider is gated behind a Cargo feature and
is **not** included in a default build. You must build with:

```
cargo build --release --features google
```

**Model note:** QuickDictate talks to the v1 endpoint and only supports v1
models (`latest_long` / `default`). Newer v2/Chirp models reject plain API-key
auth, so they are out of scope here — don't set `stt_model` to a v2/Chirp
model name for this provider.

**Pricing:** Check Google Cloud's official Speech-to-Text pricing page for
current rates and free-tier limits; both drift over time.

---

## Choosing a provider

ElevenLabs, Deepgram, OpenAI, AssemblyAI, and DashScope are all **streaming**
providers: audio goes out over a WebSocket as you talk and you see your words
appear live, word by word. **Google** is the odd one out — it's **batch**
(record-then-send over HTTPS), so you speak, then pause, and the whole
transcript for that utterance arrives at once with no live word count in
between. If you want the most immediate, "watch it type as I talk" feel, pick
one of the five streaming providers; pick Google only if you specifically want
Google's recognition quality/language coverage and can live without live
word-by-word feedback.

Whichever you choose, remember: your audio and API keys go only to the
provider you select, never to the QuickDictate maintainer. (The only thing the
app ever reports is the anonymous daily update-check ping described in
[SECURITY.md](../.github/SECURITY.md).)
