"""The audio side of elevenlabs_probe.py: building the probe's timeline from
the speech clips and synthetic noise, measuring it the way the app does, and
wrapping chunks in Scribe's input_audio_chunk envelope."""

import base64
import json
import random
import struct
import wave
from pathlib import Path

RATE = 16000
CHUNK = 1600  # 100 ms, same as the app
SILENCE_RMS = 1500  # same floor as src/stt/mod.rs


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
