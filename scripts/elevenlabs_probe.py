"""Drive ElevenLabs Scribe v2 realtime exactly the way QuickDictate does, and
log EVERY server frame with a timestamp.

Built 2026-09-11 while chasing "it stops listening": the app's log showed one
VAD commit and then silence for the rest of the press. This probe answers, in
40 seconds and without a microphone, whether the SERVER misbehaves on a given
audio shape (long noisy pause, loud room, client-side commit nudges) or whether
it is an app-side fault. Three runs that afternoon (quiet pause, noise ~1300,
noise ~2200 with a nudge) all committed every segment within ~2.5 s, which is
what pinned the failure on an intermittent server-side stall and led to the
stall watchdog in src/stt/send_task.rs.

Audio: segA, 1.5 s near-silence, segB, <pause> s of room noise, segC, 2 s
near-silence, streamed in real time as 100 ms chunks with commit_strategy=vad,
then a manual commit + close. Speech segments come from gen_probe_speech.ps1
(Windows TTS, 16 kHz mono). The API key is the first ELEVENLABS_KEYS entry of
my.keys.env (gitignored) and is never printed.

  pwsh scripts/gen_probe_speech.ps1 -OutDir $env:TEMP/qd-probe
  python scripts/elevenlabs_probe.py --dir $env:TEMP/qd-probe [--pause 12] [--noise 400]
                                     [--nudge] [--key 0] [--skip-c] [--lang en]

--nudge: after 2.5 s of client-side silence following uncommitted speech, send
         one manual commit (what a watchdog in the app would do) and see how
         fast the server answers.
"""

import argparse
import asyncio
import base64
import json
import random
import struct
import sys
import time
import wave
from pathlib import Path

import websockets

sys.stdout.reconfigure(encoding="utf-8")

KEYS_ENV = Path(__file__).resolve().parents[1] / "my.keys.env"
URL = (
    "wss://api.elevenlabs.io/v1/speech-to-text/realtime"
    "?language_code={lang}&model_id=scribe_v2_realtime&audio_format=pcm_16000&commit_strategy=vad"
)
RATE = 16000
CHUNK = 1600  # 100 ms, same as the app
SILENCE_RMS = 1500  # same floor as src/stt/mod.rs


def read_key(index: int) -> str:
    for line in KEYS_ENV.read_text().splitlines():
        if line.startswith("ELEVENLABS_KEYS="):
            keys = [k.strip() for k in line.split("=", 1)[1].split(",") if k.strip()]
            return keys[index]
    raise SystemExit(f"no ELEVENLABS_KEYS line in {KEYS_ENV}")


def load(wav_dir: Path, name: str) -> list[int]:
    with wave.open(str(wav_dir / f"{name}.wav"), "rb") as w:
        assert (
            w.getnchannels() == 1 and w.getsampwidth() == 2 and w.getframerate() == RATE
        ), w.getparams()
        raw = w.readframes(w.getnframes())
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def noise(secs: float, rms: int) -> list[int]:
    n = int(secs * RATE)
    a = int(rms * 1.732)  # uniform in [-a, a] has rms a/sqrt(3)
    return [random.randint(-a, a) for _ in range(n)]


def rms(chunk: list[int]) -> int:
    if not chunk:
        return 0
    return int((sum(x * x for x in chunk) / len(chunk)) ** 0.5)


def build(wav_dir: Path, pause: float, noise_rms: int, skip_c: bool):
    parts: list[int] = []
    marks: list[tuple[float, str]] = []
    t = 0.0

    def add(samples, label):
        nonlocal t
        marks.append((t, label))
        parts.extend(samples)
        t += len(samples) / RATE

    add(load(wav_dir, "segA"), "segA speech starts")
    add(noise(1.5, 50), "1.5 s near-silence")
    add(load(wav_dir, "segB"), "segB speech starts")
    add(noise(pause, noise_rms), f"{pause} s room noise (rms~{noise_rms})")
    if not skip_c:
        add(load(wav_dir, "segC"), "segC speech starts")
    add(noise(2.0, 50), "2 s near-silence")
    marks.append((t, "end of audio"))
    return parts, marks


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True, help="folder holding segA/segB/segC.wav")
    ap.add_argument("--pause", type=float, default=12.0)
    ap.add_argument("--nudge", action="store_true")
    ap.add_argument("--key", type=int, default=0)
    ap.add_argument("--noise", type=int, default=400)
    ap.add_argument("--skip-c", action="store_true")
    ap.add_argument("--lang", default="en")
    ap.add_argument("--linger", type=float, default=8.0)
    return ap.parse_args()


class Clock:
    """Seconds since the probe connected, as the `+  1.23s` prefix every line carries."""

    def __init__(self) -> None:
        self.t0 = time.perf_counter()

    def elapsed(self) -> float:
        return time.perf_counter() - self.t0

    def stamp(self) -> str:
        return f"+{self.elapsed():6.2f}s"


def audio_msg(chunk: list[int], commit: bool = False) -> str:
    b = struct.pack(f"<{len(chunk)}h", *chunk)
    msg = {
        "message_type": "input_audio_chunk",
        "audio_base_64": base64.b64encode(b).decode(),
        "sample_rate": RATE,
    }
    if commit:
        msg["commit"] = True
    return json.dumps(msg)


class NudgeWatch:
    """The --nudge rule: one manual commit after 2.5 s of client-side silence that
    follows uncommitted speech, re-armed by the next speech chunk."""

    def __init__(self) -> None:
        self.speech_since_commit = 0
        self.silent_run = 0.0
        self.nudged = False
        self.commits_sent = 0

    def observe(self, chunk: list[int]) -> None:
        if rms(chunk) >= SILENCE_RMS:
            self.speech_since_commit += 1
            self.silent_run = 0.0
            self.nudged = False
        else:
            self.silent_run += len(chunk) / RATE

    def due(self) -> bool:
        return not self.nudged and self.speech_since_commit > 0 and self.silent_run >= 2.5

    def sent(self) -> None:
        self.commits_sent += 1
        self.nudged = True
        self.speech_since_commit = 0


async def send_audio(ws, samples: list[int], nudge: bool, clock: Clock) -> None:
    watch = NudgeWatch()
    for i in range(0, len(samples), CHUNK):
        chunk = samples[i : i + CHUNK]
        await ws.send(audio_msg(chunk))
        watch.observe(chunk)
        if nudge and watch.due():
            await ws.send(audio_msg([], commit=True))
            watch.sent()
            print(f"{clock.stamp()} >>> client NUDGE commit #{watch.commits_sent} sent (2.5 s client silence)")
        await asyncio.sleep(0.1)
    print(f"{clock.stamp()} >>> audio done; sending final manual commit")
    await ws.send(audio_msg([], commit=True))
    await asyncio.sleep(0.3)  # PRE_CLOSE_DELAY in src/stt/elevenlabs.rs
    print(f"{clock.stamp()} >>> sending WS close")
    await ws.close()


def describe_frame(d: dict) -> str:
    """One server frame as the probe prints it."""
    mt = d.get("message_type")
    text = d.get("text") or d.get("committed_transcript") or ""
    if mt == "partial_transcript":
        return f"partial ({len(text.split())} words): {text[-70:]!r}"
    if mt and mt.startswith("committed_transcript"):
        return f"*** COMMITTED: {text!r}"
    if mt == "session_started":
        cfg = {k: v for k, v in d.items() if k != "message_type"}
        return f"session_started {json.dumps(cfg)[:400]}"
    return f"OTHER: {json.dumps(d)[:400]}"


async def receive_frames(ws, clock: Clock, events: list[tuple[float, str | None]]) -> None:
    try:
        async for raw in ws:
            try:
                d = json.loads(raw)
            except Exception:
                print(f"{clock.stamp()} <<< non-JSON frame: {str(raw)[:120]!r}")
                continue
            events.append((clock.elapsed(), d.get("message_type")))
            print(f"{clock.stamp()} <<< {describe_frame(d)}")
    except websockets.ConnectionClosed as e:
        print(f"{clock.stamp()} <<< connection closed by peer: code={e.code} reason={e.reason!r}")


async def main() -> None:
    args = parse_args()
    key = read_key(args.key)
    samples, marks = build(Path(args.dir), args.pause, args.noise, args.skip_c)
    print(f"audio: {len(samples) / RATE:.1f} s total; timeline:")
    for t, label in marks:
        print(f"  +{t:6.2f}s  {label}")

    clock = Clock()
    url = URL.format(lang=args.lang)
    async with websockets.connect(
        url, additional_headers={"xi-api-key": key}, max_size=None
    ) as ws:
        print(f"{clock.stamp()} connected")
        events: list[tuple[float, str | None]] = []
        recv = asyncio.create_task(receive_frames(ws, clock, events))
        await send_audio(ws, samples, args.nudge, clock)
        try:
            await asyncio.wait_for(recv, timeout=args.linger)
        except asyncio.TimeoutError:
            print(f"{clock.stamp()} receiver still open after {args.linger} s linger; giving up")
            recv.cancel()
        commits = sum(1 for _, m in events if m and m.startswith("committed"))
        partials = sum(1 for _, m in events if m == "partial_transcript")
        print(f"{clock.stamp()} done. commits received: {commits}, partials: {partials}")


if __name__ == "__main__":
    asyncio.run(main())
