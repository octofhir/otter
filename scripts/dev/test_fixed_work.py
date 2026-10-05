#!/usr/bin/env python3
"""Correctness checks for measurement parsing, validation and capture bookkeeping.

All engines and resource measurements are mocked; these tests run no benchmarks.
"""
from contextlib import ExitStack, redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("fixed_work", Path(__file__).with_name("fixed-work.py"))
FW = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FW)
COUNTERS = """        0.13 real         0.10 user         0.01 sys
            55558144  maximum resident set size
          1934513003  instructions retired
"""


class FixedWorkTests(unittest.TestCase):
    def sample(self, **changes):
        sample = dict(exit=0, timedOut=False, launchToExitSeconds=0.13, **FW.parse_counters(COUNTERS))
        sample.update(changes)
        return sample

    def test_parses_wall_and_resource_counters(self):
        self.assertEqual(FW.parse_counters(COUNTERS), {
            "real_seconds": 0.13, "user_seconds": 0.10, "sys_seconds": 0.01,
            "instructions": 1934513003, "rss_bytes": 55558144})
        self.assertTrue(all(value is None for value in FW.parse_counters("missing").values()))
        self.assertIsNone(FW.parse_counters("1.2.3 real 0.1 user 0.1 sys")["real_seconds"])

    def test_exact_validation_and_explicit_line_endings(self):
        sample = self.sample()
        FW.validate_sample(sample, b"33\r\n", b"33\n", "exact")
        self.assertEqual(sample["status"], "failed")
        self.assertIn("stdout mismatch", sample["failures"])
        normalized = FW.normalize_stdout(b"33\r\n", "line-endings")
        self.assertEqual(normalized, b"33\n")
        FW.validate_sample(sample, b"33\r\n", b"33\n", "line-endings")
        self.assertEqual(sample["status"], "ok")
        FW.validate_sample(sample, b"33 \n", b"33\n", "line-endings")
        self.assertEqual(sample["status"], "failed")

    def test_timeouts_exits_and_missing_counters_are_failures(self):
        for changes, message in [({"timedOut": True}, "external watchdog timeout"),
                                 ({"exit": 1}, "exit 1"),
                                 ({"instructions": None}, "missing counters: instructions")]:
            with self.subTest(changes=changes):
                sample = self.sample(**changes)
                FW.validate_sample(sample, b"33\n", None, "exact")
                self.assertEqual(sample["status"], "failed")
                self.assertIn(message, sample["failures"])

    def test_distribution_excludes_failed_samples(self):
        samples = [dict(status="ok", **self.sample(instructions=value)) for value in [10, 20, 40]]
        samples.append(dict(status="failed", **self.sample(instructions=1000)))
        self.assertEqual(FW.summarize(samples)["instructions"],
                         {"min": 10, "median": 20, "max": 40, "count": 3})

    def test_timeout_kills_child_process_group_and_retains_outputs(self):
        process = Mock(pid=4321)
        process.wait.return_value = -9
        process.poll.return_value = None
        watchdog = Mock()
        callbacks = []

        def timer(interval, callback):
            callbacks.append(callback)
            watchdog.start.side_effect = callback
            return watchdog

        with tempfile.TemporaryDirectory() as scratch:
            prefix = Path(scratch) / "timeout"
            with patch.object(FW.subprocess, "Popen", return_value=process) as spawn, \
                    patch.object(FW.os, "killpg") as kill, \
                    patch.object(FW.threading, "Timer", side_effect=timer):
                sample = FW.capture(["fake"], prefix, 0.01)
                callbacks[0]()  # A late callback cannot kill a reused process group.
            kill.assert_called_once_with(4321, FW.signal.SIGKILL)
            process.wait.assert_called_once_with()
            watchdog.cancel.assert_called_once_with()
            watchdog.join.assert_called_once_with()
            self.assertTrue(spawn.call_args.kwargs["start_new_session"])
            self.assertTrue(sample["timedOut"])
            self.assertEqual(sample["exit"], -9)
            self.assertTrue(prefix.with_suffix(".stdout").exists())
            self.assertTrue(prefix.with_suffix(".time").exists())

    def test_launch_window_uses_blocking_wait_and_preserves_time_counter(self):
        process = Mock(pid=4321)
        process.wait.return_value = 0
        watchdog = Mock()
        with tempfile.TemporaryDirectory() as scratch:
            prefix = Path(scratch) / "wall"

            def spawn(command, **options):
                options["stderr"].write(COUNTERS.encode())
                return process

            with patch.object(FW.subprocess, "Popen", side_effect=spawn), \
                    patch.object(FW.threading, "Timer", return_value=watchdog) as timer, \
                    patch.object(FW.time, "perf_counter", side_effect=[5.0, 5.0, 5.123456, 5.14]):
                sample = FW.capture(["fake"], prefix, 7)
        process.wait.assert_called_once_with()
        self.assertAlmostEqual(sample["launchToExitSeconds"], 0.123456)
        self.assertEqual(sample["real_seconds"], 0.13)
        self.assertEqual(timer.call_args.args[0], 7)
        self.assertEqual([call[0] for call in watchdog.mock_calls], ["start", "cancel", "join"])

    def test_natural_exit_at_watchdog_deadline_does_not_become_timeout(self):
        process = Mock(pid=4321)
        process.wait.return_value = 0
        process.poll.return_value = 0
        watchdog = Mock()

        def timer(interval, callback):
            watchdog.start.side_effect = callback
            return watchdog

        with tempfile.TemporaryDirectory() as scratch:
            with patch.object(FW.subprocess, "Popen", return_value=process), \
                    patch.object(FW.threading, "Timer", side_effect=timer), \
                    patch.object(FW.os, "killpg") as kill:
                sample = FW.capture(["fake"], Path(scratch) / "exited", 0.01)
        kill.assert_not_called()
        self.assertFalse(sample["timedOut"])
        self.assertEqual(sample["exit"], 0)

    def run_main(self, scratch, *, missing=False, mismatch=False, changed_tree=False,
                 reference=False, missing_reference_path=False):
        root = Path(scratch)
        (root / "fib.js").write_text("print(33)")
        executables = {}
        selected = ["otter", "otter-reference", "node", "bun"] if reference else ["otter", "node", "bun"]
        for engine in selected:
            executable = root / engine
            executable.write_text("fake executable " + engine)
            executable.chmod(0o755)
            executables[engine] = str(executable)
        calls = []
        occurrences = {}

        def capture(command, prefix, timeout):
            engine = Path(command[0]).name
            calls.append(engine)
            occurrences[engine] = occurrences.get(engine, 0) + 1
            output = b"different\n" if mismatch and engine == "bun" else b"33\n"
            prefix.with_suffix(".stdout").write_bytes(output)
            prefix.with_suffix(".time").write_text(COUNTERS)
            return dict(command=command, stdoutFile=prefix.with_suffix(".stdout").name,
                        **self.sample(instructions=occurrences[engine] * 10))

        provenance = {"treeSha256": "before", "dirty": True, "head": "test-head"}
        output = root / "out"
        argv = ["fixed-work.py", str(output), "--otter", executables["otter"],
                "--engines", *selected, "--workloads", "fib"]
        if reference and not missing_reference_path:
            argv.extend(["--reference-otter", executables["otter-reference"]])
        with ExitStack() as stack:
            stack.enter_context(patch.object(FW, "ROOT", root))
            stack.enter_context(patch.object(FW, "WORKLOADS", {"fib": "fib.js"}))
            stack.enter_context(patch.object(FW, "source_provenance", side_effect=[
                provenance, dict(provenance, treeSha256="after" if changed_tree else "before")]))
            stack.enter_context(patch.object(FW.shutil, "which", side_effect=lambda engine:
                                            None if missing and engine == "bun" else executables[engine]))
            stack.enter_context(patch.object(FW.subprocess, "check_output", return_value="fake 1.0\n"))
            stack.enter_context(patch.object(FW, "capture", side_effect=capture))
            stack.enter_context(patch.object(FW.sys, "argv", argv))
            stack.enter_context(patch.object(FW.sys, "platform", "darwin"))
            stack.enter_context(patch.dict(FW.os.environ, {}, clear=True))
            stack.enter_context(redirect_stdout(io.StringIO()))
            stack.enter_context(redirect_stderr(io.StringIO()))
            result = FW.main()
        return result, json.loads((output / "results.json").read_text()), \
            json.loads((output / "environment.json").read_text()), calls

    def test_default_repeats_exclude_warmups_and_rotate_engines(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, calls = self.run_main(scratch)
        self.assertEqual(result, 0)
        self.assertEqual(calls[:9], ["otter", "node", "bun", "otter", "node", "bun", "node", "bun", "otter"])
        self.assertEqual(len(calls), 18)
        for row in rows:
            self.assertEqual(len(row["samples"]), 5)
            self.assertEqual(len(row["warmup_samples"]), 1)
            self.assertEqual(row["instructions"], 40)
            self.assertEqual(row["summaries"]["instructions"], {"min": 20, "median": 40, "max": 60, "count": 5})
            self.assertTrue(row["scoreable"])
        self.assertEqual(metadata["status"], "ok")
        self.assertFalse(metadata["baselineEligible"])
        self.assertIsNone(metadata["executables"]["otter"]["sourceRevision"])
        self.assertIn("eval", rows[2]["command"][2])

    def test_missing_engine_fails_before_measurement(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, calls = self.run_main(scratch, missing=True)
        self.assertEqual(result, 1)
        self.assertEqual(calls, [])
        self.assertEqual(metadata["executables"]["bun"]["status"], "unavailable")
        self.assertTrue(all(not row["scoreable"] for row in rows))

    def test_reference_uses_the_same_otter_command_semantics(self):
        source = Path("fixture.js")
        self.assertEqual(FW.build_command("otter", "candidate", source), ["candidate", "run", "fixture.js"])
        self.assertEqual(FW.build_command("otter-reference", "reference", source), ["reference", "run", "fixture.js"])
        self.assertEqual(FW.build_command("node", "node", source), ["node", "fixture.js"])

    def test_reference_without_path_fails_before_measurement(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, calls = self.run_main(scratch, reference=True, missing_reference_path=True)
        self.assertEqual(result, 1)
        self.assertEqual(calls, [])
        self.assertEqual(metadata["executables"]["otter-reference"]["status"], "unavailable")
        self.assertIsNone(metadata["executables"]["otter-reference"]["path"])
        self.assertIn("--reference-otter PATH", metadata["executables"]["otter-reference"]["failure"])
        self.assertTrue(all(not row["scoreable"] for row in rows))

    def test_reference_pairs_rotate_with_candidate_and_keep_distinct_provenance(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, calls = self.run_main(scratch, reference=True)
        self.assertEqual(result, 0)
        engines = ["otter", "otter-reference", "node", "bun"]
        self.assertEqual(calls[:4], engines)
        measured = calls[4:]
        self.assertEqual(len(measured), 20)
        for round_index in range(5):
            offset = round_index % 4
            self.assertEqual(measured[round_index * 4:(round_index + 1) * 4], engines[offset:] + engines[:offset])
        for row in rows:
            self.assertEqual(len(row["samples"]), 5)
            self.assertEqual(len(row["warmup_samples"]), 1)
            self.assertTrue(row["scoreable"])
            self.assertEqual([sample["round"] for sample in row["samples"]], [1, 2, 3, 4, 5])
        candidate = metadata["executables"]["otter"]
        reference = metadata["executables"]["otter-reference"]
        self.assertNotEqual(candidate["path"], reference["path"])
        self.assertNotEqual(candidate["sha256"], reference["sha256"])
        self.assertIsNone(candidate["sourceRevision"])
        self.assertIsNone(reference["sourceRevision"])
        self.assertEqual(candidate["buildProfile"], "unverified")
        self.assertEqual(reference["buildProfile"], "unverified")

    def test_mismatch_fails_capture_and_keeps_raw_evidence(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, calls = self.run_main(scratch, mismatch=True)
            self.assertEqual((Path(scratch) / "out/fib-bun-warmup-0001.stdout").read_bytes(), b"different\n")
        self.assertEqual(result, 1)
        self.assertEqual(calls, ["otter", "node", "bun"])
        self.assertEqual(metadata["status"], "failed")
        self.assertFalse(rows[2]["warmup_samples"][0]["matchesExpectedStdout"])
        self.assertTrue(all(not row["scoreable"] for row in rows))

    def test_changed_tree_invalidates_all_rows(self):
        with tempfile.TemporaryDirectory() as scratch:
            result, rows, metadata, _ = self.run_main(scratch, changed_tree=True)
        self.assertEqual(result, 1)
        self.assertIn("source tree changed during measurement", metadata["failures"])
        self.assertTrue(all(not row["scoreable"] for row in rows))

    def test_existing_output_directory_is_never_reused(self):
        with tempfile.TemporaryDirectory() as scratch:
            with patch.object(FW.sys, "argv", ["fixed-work.py", scratch]):
                with self.assertRaises(FileExistsError):
                    FW.main()


if __name__ == "__main__":
    unittest.main()
