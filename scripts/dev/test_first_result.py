#!/usr/bin/env python3
"""Real process and pipe proofs for first-result captures; no engine timings."""
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("first_result", Path(__file__).with_name("first-result.py"))
CAPTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CAPTURE)
LINE = b"fixed-first-result\ttiny-js\tundefined\n"


class FirstResultTests(unittest.TestCase):
    def setUp(self):
        state = patch.object(CAPTURE, "_termination", None)
        state.start()
        self.addCleanup(state.stop)

    def run_child(self, source, timeout=2):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        prefix = Path(scratch.name) / "capture"
        record = CAPTURE.capture([sys.executable, "-B", "-c", source], prefix, LINE, timeout)
        self.assertEqual(record["stdoutSha256"], CAPTURE.digest(prefix.with_suffix(".stdout")))
        self.assertEqual(record["stderrSha256"], CAPTURE.digest(prefix.with_suffix(".stderr")))
        return record, prefix

    def test_split_result_has_one_timestamp_before_later_process_exit(self):
        source = "import os,time; os.write(1," + repr(LINE[:10]) + "); time.sleep(.03); os.write(1," + repr(LINE[10:]) + "); time.sleep(.12)"
        record, _ = self.run_child(source)
        self.assertEqual(record["status"], "passed")
        self.assertGreater(record["firstValidatedResultNs"], 0)
        self.assertGreater(record["launchToExitNs"] - record["firstValidatedResultNs"], 60_000_000)

    def test_complete_line_followed_by_failed_exit_retains_observation_without_score(self):
        record, prefix = self.run_child("import os,sys; os.write(1," + repr(LINE) + "); sys.exit(7)")
        self.assertEqual(record["exit"], 7)
        self.assertEqual(prefix.with_suffix(".stdout").read_bytes(), LINE)
        self.assertIsNotNone(record["observedResultNs"])
        self.assertIsNone(record["firstValidatedResultNs"])
        self.assertIsNone(record["launchToExitNs"])

    def test_duplicate_or_trailing_output_never_scores(self):
        for suffix in (LINE, b"unexpected\n"):
            with self.subTest(suffix=suffix):
                record, _ = self.run_child("import os; os.write(1," + repr(LINE + suffix) + ")")
                self.assertEqual(record["status"], "failed")
                self.assertIsNotNone(record["observedResultNs"])
                self.assertIsNone(record["firstValidatedResultNs"])

    def test_wrong_or_unterminated_result_never_scores(self):
        for output in (LINE[:-1], b"wrong\n", b"prefix\n" + LINE, b""):
            with self.subTest(output=output):
                record, prefix = self.run_child("import os; os.write(1," + repr(output) + ")")
                self.assertEqual(record["status"], "failed")
                self.assertIsNone(record["firstValidatedResultNs"])
                self.assertEqual(prefix.with_suffix(".stdout").read_bytes(), output)

    def child_with_descendant(self, marker, leader_exits):
        child = "import time; from pathlib import Path; time.sleep(.7); Path(" + repr(str(marker)) + ").write_text('survived')"
        leader = "import subprocess,sys,os,time; subprocess.Popen([sys.executable,'-B','-c'," + repr(child) + "]); os.write(1," + repr(LINE) + "); "
        return leader + ("" if leader_exits else "time.sleep(60)")

    def test_deadline_ends_owned_group_even_after_valid_result(self):
        with tempfile.TemporaryDirectory() as scratch:
            marker = Path(scratch) / "descendant"
            record, _ = self.run_child(self.child_with_descendant(marker, False), timeout=.2)
            self.assertTrue(record["timedOut"])
            self.assertIsNotNone(record["observedResultNs"])
            self.assertIsNone(record["firstValidatedResultNs"])
            time.sleep(.75)
            self.assertFalse(marker.exists(), "owned descendant escaped the deadline")

    def test_successful_leader_cannot_leave_a_pipe_or_descendant_alive(self):
        with tempfile.TemporaryDirectory() as scratch:
            marker = Path(scratch) / "descendant"
            record, _ = self.run_child(self.child_with_descendant(marker, True))
            self.assertEqual(record["exit"], 0)
            self.assertFalse(record["timedOut"])
            self.assertTrue(record["residualProcessGroupTerminated"])
            self.assertIsNone(record["firstValidatedResultNs"])
            time.sleep(.75)
            self.assertFalse(marker.exists(), "owned descendant survived successful leader completion")

    def test_spawn_failure_keeps_raw_files_and_stops_future_groups(self):
        with tempfile.TemporaryDirectory() as scratch:
            prefix = Path(scratch) / "missing"
            record = CAPTURE.capture([str(Path(scratch) / "absent-executable")], prefix, LINE, 1)
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["waitFailure"], "FileNotFoundError")
            self.assertTrue(prefix.with_suffix(".stdout").is_file())
            self.assertTrue(prefix.with_suffix(".stderr").is_file())
            with patch.object(CAPTURE.subprocess, "Popen") as later:
                with self.assertRaisesRegex(ValueError, "capture terminated"):
                    CAPTURE.capture(["must-not-run"], prefix.with_name("next"), LINE, 1)
                later.assert_not_called()

    def test_missing_or_nonfinite_deadline_rejects_before_spawn(self):
        with tempfile.TemporaryDirectory() as scratch:
            for timeout in (0, -1, float("inf"), float("nan")):
                with self.subTest(timeout=timeout), patch.object(CAPTURE.subprocess, "Popen") as spawn:
                    with self.assertRaises(ValueError):
                        CAPTURE.capture(["must-not-run"], Path(scratch) / "invalid", LINE, timeout)
                    spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
