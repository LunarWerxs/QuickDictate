"""spans_report.py prints median and p90 from a spans.jsonl (python -m unittest scripts/test_spans_report.py)."""
import json, os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "spans_report.py")


def row(key_release, paste_done, msgs):
    return {"epoch": 1, "provider": "deepgram", "key_release_ms": key_release,
            "paste_done_ms": paste_done, "stt_messages": msgs, "polish_tokens_in": 0,
            "polish_tokens_out": 0, "audio_bytes_sent": 0}


class SpansReport(unittest.TestCase):
    def run_report(self, *args):
        # release-to-paste: 100..1000 in steps of 100 -> median 550, p90 900
        rows = [row(1000, 1000 + 100 * i, i) for i in range(1, 11)]
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "spans.jsonl")
            with open(path, "w", encoding="utf-8") as f:
                f.write("not json\n")
                f.writelines(json.dumps(r) + "\n" for r in rows)
            return subprocess.run([sys.executable, SCRIPT, "--file", path, *args],
                                  capture_output=True, text=True, check=True).stdout

    def test_json_has_median_and_p90(self):
        out = json.loads(self.run_report("--json"))
        self.assertEqual(out["release_to_paste_ms"], {"n": 10, "p50": 550.0, "p90": 900})
        self.assertEqual(out["stt_messages"]["p50"], 5.5)

    def test_table_prints_both_columns(self):
        out = self.run_report()
        line = next(l for l in out.splitlines() if l.startswith("release_to_paste_ms"))
        self.assertEqual(line.split()[1:], ["10", "550", "900"])


if __name__ == "__main__":
    unittest.main()
