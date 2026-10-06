#!/usr/bin/env python3
"""Median and p90 of each latency span over the last N dictations.

Reads logs/spans.jsonl (written by QuickDictate, one line per dictation, no
dictated text). Usage:

  python scripts/spans_report.py [-n 50] [--file PATH] [--json]

PATH defaults to $QUICKDICTATE_SPANS, else logs/spans.jsonl in the current
folder, else beside this repo's exe. --json prints a flat object (for
.rsi/rsi.yaml: 'json:$.release_to_paste_ms.p50').
"""
import argparse, json, math, os, sys

SPANS = ["connected_ms", "key_release_ms", "stt_tail_ms", "final_transcript_ms",
         "polish_done_ms", "paste_done_ms"]
COUNTS = ["stt_messages", "polish_tokens_in", "polish_tokens_out", "audio_bytes_sent"]


def pct(xs, p):
    xs = sorted(xs)
    return xs[max(0, math.ceil(p / 100 * len(xs)) - 1)]


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def find_file(arg):
    here = os.path.dirname(os.path.abspath(__file__))
    for p in (arg, os.environ.get("QUICKDICTATE_SPANS"), os.path.join("logs", "spans.jsonl"),
              os.path.join(here, "..", "logs", "spans.jsonl")):
        if p and os.path.isfile(p):
            return p
    sys.exit("no spans.jsonl found; pass --file")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-n", type=int, default=50)
    ap.add_argument("--file")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    rows = []
    with open(find_file(a.file), encoding="utf-8") as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except ValueError:
                pass
    rows = rows[-a.n:]
    for r in rows:
        k, p = r.get("key_release_ms"), r.get("paste_done_ms")
        r["release_to_paste_ms"] = p - k if k is not None and p is not None else None
    out = {}
    for name in SPANS + ["release_to_paste_ms"] + COUNTS:
        xs = [r[name] for r in rows if r.get(name) is not None]
        if xs:
            out[name] = {"n": len(xs), "p50": median(xs), "p90": pct(xs, 90)}
    if a.json:
        print(json.dumps(out))
        return
    print(f"last {len(rows)} dictations")
    print(f"{'span (ms from key-down)':28}{'n':>5}{'median':>9}{'p90':>9}")
    for name, s in out.items():
        print(f"{name:28}{s['n']:>5}{s['p50']:>9.0f}{s['p90']:>9.0f}")
    print("\nper dictation:")
    print(f"{'epoch':>6} {'provider':10}{'r2p ms':>8}" + "".join(f"{c[:14]:>16}" for c in COUNTS))
    for r in rows:
        r2p = r["release_to_paste_ms"]
        print(f"{r.get('epoch', 0):>6} {str(r.get('provider', ''))[:9]:10}"
              f"{'-' if r2p is None else r2p:>8}" + "".join(f"{r.get(c, 0):>16}" for c in COUNTS))


main()
