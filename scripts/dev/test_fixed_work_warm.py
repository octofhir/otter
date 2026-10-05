#!/usr/bin/env python3
"""Exercise real owned-process lifetime and reject incomplete warm evidence.

The subprocess fixtures are Python sleepers, not benchmark engines. Identity
and semantic tests use temporary owned files and complete synthetic records.
"""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("warm_capture", Path(__file__).with_name("fixed-work-warm.py"))
WARM = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WARM)


class WarmCaptureTests(unittest.TestCase):
    def setUp(self):
        self.termination = patch.object(WARM, "_capture_termination", None)
        self.termination.start()
        self.addCleanup(self.termination.stop)

    def child_command(self, marker, exit_leader):
        child = "import time; from pathlib import Path; time.sleep(1); Path(" + repr(str(marker)) + ").write_text('survived')"
        leader = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," + repr(child) + "]); "
        leader += "print('started',flush=True); " + ("" if exit_leader else "time.sleep(60)")
        return [sys.executable, "-B", "-c", leader]

    def test_watchdog_kills_owned_group_and_keeps_raw_output(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            marker = directory / "orphan-marker"
            record = WARM.capture(self.child_command(marker, False), directory / "deadline", 0.4)
            self.assertTrue(record["timedOut"])
            self.assertNotEqual(record["exit"], 0)
            self.assertIn(b"started", (directory / record["stdout"]).read_bytes())
            self.assertEqual(record["stdoutSha256"], WARM.digest(directory / record["stdout"]))
            time.sleep(1.1)
            self.assertFalse(marker.exists(), "owned descendant survived the process-group deadline")

    def test_successful_leader_with_live_descendant_is_rejected_and_terminated(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            marker = directory / "orphan-marker"
            record = WARM.capture(self.child_command(marker, True), directory / "leader", 10)
            self.assertEqual(record["exit"], 0)
            self.assertFalse(record["timedOut"])
            self.assertTrue(record["residualProcessGroupTerminated"])
            time.sleep(1.1)
            self.assertFalse(marker.exists(), "completed leader leaked its owned descendant")

    def test_late_watchdog_callback_never_kills_after_capture_returns(self):
        process = Mock(pid=4321)
        process.wait.return_value = 0
        watchdog = Mock()
        callbacks = []

        def timer(interval, callback):
            callbacks.append(callback)
            return watchdog

        with tempfile.TemporaryDirectory() as scratch:
            with patch.object(WARM.subprocess, "Popen", return_value=process), \
                    patch.object(WARM.threading, "Timer", side_effect=timer), \
                    patch.object(WARM.os, "killpg", side_effect=ProcessLookupError) as kill:
                record = WARM.capture(["owned-fixture"], Path(scratch) / "natural", 10)
                callbacks[0]()
            self.assertFalse(record["timedOut"])
            self.assertFalse(record["residualProcessGroupTerminated"])
            kill.assert_called_once_with(4321, 0)
            process.wait.assert_called_once_with()
            watchdog.cancel.assert_called_once_with()
            watchdog.join.assert_called_once_with()

    def test_changed_captured_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "validator"
            path.write_bytes(b"first identity")
            identities = {"validator": WARM.file_identity(path)}
            WARM.verify_files(identities)
            path.write_bytes(b"different identity")
            with self.assertRaisesRegex(ValueError, "changed"):
                WARM.verify_files(identities)

    def test_presence_enabled_diagnostics_reject_zero_and_empty_values(self):
        for value in ("", "0", "1"):
            with self.subTest(value=value), patch.dict(WARM.os.environ, {"OTTER_JIT_TRACE": value}, clear=True):
                with self.assertRaisesRegex(ValueError, "OTTER_JIT_TRACE"):
                    WARM.reject_diagnostics()

    def test_interrupted_wait_kills_and_reaps_before_watchdog_cancel(self):
        process = Mock(pid=4321)
        process.wait.side_effect = [KeyboardInterrupt, -9]
        watchdog = Mock()
        with tempfile.TemporaryDirectory() as scratch:
            with patch.object(WARM.subprocess, "Popen", return_value=process), \
                    patch.object(WARM.threading, "Timer", return_value=watchdog), \
                    patch.object(WARM.os, "killpg", return_value=None) as kill:
                record = WARM.capture(["owned-fixture"], Path(scratch) / "interrupted", 10)
            self.assertEqual(record["waitFailure"], "KeyboardInterrupt")
            self.assertEqual(process.wait.call_count, 2)
            self.assertEqual(kill.call_args_list[0].args, (4321, WARM.signal.SIGKILL))
            kill.assert_called_once_with(4321, WARM.signal.SIGKILL)
            watchdog.cancel.assert_called_once_with()
            watchdog.join.assert_called_once_with()
            with patch.object(WARM.subprocess, "Popen") as later_spawn:
                with self.assertRaisesRegex(ValueError, "capture terminated"):
                    WARM.capture(["must-not-start"], Path(scratch) / "later", 10)
            later_spawn.assert_not_called()

    def test_failed_group_probe_preserves_invalid_evidence_and_stops_capture(self):
        process = Mock(pid=4321)
        process.wait.return_value = 0
        watchdog = Mock()
        with tempfile.TemporaryDirectory() as scratch:
            with patch.object(WARM.subprocess, "Popen", return_value=process), \
                    patch.object(WARM.threading, "Timer", return_value=watchdog), \
                    patch.object(WARM.os, "killpg", side_effect=[PermissionError, None]) as kill:
                record = WARM.capture(["owned-fixture"], Path(scratch) / "refused", 10)
            self.assertEqual(record["waitFailure"], "group-probe:PermissionError")
            self.assertTrue(record["residualProcessGroupTerminated"])
            self.assertEqual(len(record["stdoutSha256"]), 64)
            self.assertEqual(kill.call_args_list[1].args, (4321, WARM.signal.SIGKILL))
            with patch.object(WARM.subprocess, "Popen") as later_spawn:
                with self.assertRaisesRegex(ValueError, "capture terminated"):
                    WARM.capture(["must-not-start"], Path(scratch) / "later", 10)
            later_spawn.assert_not_called()

    def test_failed_block_is_retained_and_cannot_become_a_score(self):
        row = {"blocks": [{"status": "passed", "medianNs": 100, "measuredNs": [100] * 5},
                          {"status": "failed", "medianNs": 1000000}],
               "failures": ["block 2 timeout"]}
        WARM.summarize(row, 2)
        self.assertFalse(row["scoreable"])
        self.assertEqual(row["status"], "failed")
        self.assertEqual(len(row["blocks"]), 2)
        self.assertEqual(row["summary"]["blocks"], 1)
        self.assertEqual(row["summary"]["invocations"], 5)

    def test_same_process_invocations_do_not_inflate_independent_block_count(self):
        row = {"blocks": [{"status": "passed", "medianNs": 100, "measuredNs": [100] * 150}], "failures": []}
        WARM.summarize(row, 5)
        self.assertFalse(row["scoreable"])
        self.assertEqual(row["summary"]["blocks"], 1)
        self.assertEqual(row["summary"]["invocations"], 150)

    def test_same_median_with_overlapping_blocks_is_not_a_victory(self):
        rows = [{"anchor": "fib", "engine": "otter", "scoreable": True,
                 "summary": {"medianNs": 80, "minNs": 40, "maxNs": 120}},
                {"anchor": "fib", "engine": "bun", "scoreable": True,
                 "summary": {"medianNs": 100, "minNs": 60, "maxNs": 140}}]
        self.assertEqual(WARM.comparison(rows, "fib")["status"], "overlap")
        rows[0]["scoreable"] = False
        self.assertEqual(WARM.comparison(rows, "fib")["status"], "coverage-gap")

    def test_positive_report_label_cannot_replace_missing_semantic_rows(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            validator = directory / "validator"
            validator.write_bytes(b"validator")
            identity = {"scriptSha256": "generated", "manifestSha256": "manifest",
                        "originalScriptSha256": "original-script", "originalSha256": "original"}
            report = {"status": "passed", "purpose": "untimed original/generated semantic validation", "failures": [],
                      "source": {"treeSha256": "tree"}, "sourceAfter": {"treeSha256": "tree"},
                      "files": {"validator": WARM.file_identity(validator)}, "prepared": {"fib": identity},
                      "rows": [], "executables": {}}
            path = directory / "semantics.json"
            WARM.save_json(path, report)
            with self.assertRaisesRegex(ValueError, "missing original/generated"):
                WARM.verify_semantics(path, {"treeSha256": "tree"}, {"fib": identity}, {}, validator)
            report["rows"] = [{"anchor": "fib", "engine": engine, "phase": phase, "status": "passed"}
                              for engine in ("otter-interpreter", "otter", "bun", "node")
                              for phase in ("original", "generated")]
            stdout = directory / "semantic.stdout"
            stderr = directory / "semantic.stderr"
            stdout.write_bytes(b"retained complete stdout")
            stderr.write_bytes(b"")
            raw = {"exit": 0, "timedOut": False, "waitFailure": None, "residualProcessGroupTerminated": False,
                   "stdout": stdout.name, "stderr": stderr.name,
                   "stdoutSha256": WARM.digest(stdout), "stderrSha256": WARM.digest(stderr)}
            validated = directory / "semantic.validated.json"
            WARM.save_json(validated, {"anchor": "fib", "warmups": [1] * 3, "samples": [1] * 5})
            for row in report["rows"]:
                row.update(raw)
                if row["phase"] == "generated":
                    row.update(validation=raw, validatedFile=validated.name, validatedSha256=WARM.digest(validated))
            WARM.save_json(path, report)
            WARM.verify_semantics(path, {"treeSha256": "tree"}, {"fib": identity}, {}, validator)
            stdout.write_bytes(b"changed retained output")
            with self.assertRaisesRegex(ValueError, "raw semantic evidence changed"):
                WARM.verify_semantics(path, {"treeSha256": "tree"}, {"fib": identity}, {}, validator)
            stdout.write_bytes(b"retained complete stdout")
            report["rows"][0]["status"] = "failed"
            WARM.save_json(path, report)
            with self.assertRaisesRegex(ValueError, "failed original/generated"):
                WARM.verify_semantics(path, {"treeSha256": "tree"}, {"fib": identity}, {}, validator)


if __name__ == "__main__":
    unittest.main()
