#!/usr/bin/env python3
"""Observe a complete validated result on stdout, with an owned process deadline.

The timestamp is parent-observed delivery of the exact predeclared result line,
including launch, useful work and stdout transport. A later failure, duplicate
result, leftover descendant, timeout or changed identity invalidates the sample.
Raw output and the observed timestamp remain evidence even for invalid samples.
This module does not infer an engine's GC state or peak RSS from elapsed time.
"""
import hashlib
import math
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True
_termination = None


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def capture(command, prefix, expected_line, timeout, *, cwd=None, env=None):
    """Keep exact raw bytes; only a completed, validated process has metrics.

    `expected_line` includes one final newline, and is the whole stdout contract
    for these startup cases. Every call launches and owns a fresh session. The
    timeout bounds the session, including inherited pipes held by descendants.
    """
    global _termination
    if _termination is not None:
        raise ValueError("capture terminated: " + _termination)
    if (not isinstance(expected_line, bytes) or not expected_line.endswith(b"\n")
            or b"\n" in expected_line[:-1] or not expected_line[:-1]):
        raise ValueError("expected one complete nonempty result line")
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("expected a finite positive process deadline")
    prefix = Path(prefix)
    if not prefix.parent.is_dir():
        raise ValueError("raw output directory must already exist")
    started = time.perf_counter_ns()
    deadline = started + math.ceil(timeout * 1e9)
    observed = None
    output_length = 0
    candidate = bytearray()
    failure = None
    timed_out = False
    residual = False
    code = None
    process = None

    def kill_group():
        global _termination
        nonlocal failure
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            failure = "group-kill:" + type(error).__name__
            _termination = failure
            try:
                process.kill()
            except ProcessLookupError:
                pass

    with prefix.with_suffix(".stdout").open("xb") as out, prefix.with_suffix(".stderr").open("xb") as err:
        try:
            process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=err, start_new_session=True)
            with selectors.DefaultSelector() as selector:
                os.set_blocking(process.stdout.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ)
                while selector.get_map():
                    remaining = (deadline - time.perf_counter_ns()) / 1e9
                    if remaining <= 0:
                        timed_out = True
                        kill_group()
                        break
                    # Polling only matters for a leader whose descendant keeps
                    # stdout open; ordinary completion wakes the pipe with EOF.
                    events = selector.select(min(remaining, 0.05))
                    for key, _ in events:
                        try:
                            chunk = os.read(key.fd, 65536)
                        except BlockingIOError:
                            continue
                        arrived = time.perf_counter_ns()
                        if not chunk:
                            selector.unregister(key.fileobj)
                            break
                        output_length += len(chunk)
                        if observed is None and len(candidate) < len(expected_line):
                            candidate.extend(chunk[:len(expected_line) - len(candidate)])
                            if bytes(candidate) == expected_line:
                                observed = arrived - started
                        out.write(chunk)
                    if process.poll() is not None and selector.get_map():
                        # A live pipe after leader exit may have unread bytes.
                        # Reject any live owned descendant, kill it, then drain
                        # the pipe normally so its final raw bytes are retained.
                        try:
                            os.killpg(process.pid, 0)
                        except ProcessLookupError:
                            pass
                        except OSError as error:
                            failure = "group-probe:" + type(error).__name__
                            _termination = failure
                            residual = True
                            kill_group()
                        else:
                            residual = True
                            kill_group()
                remaining = max(0.001, (deadline - time.perf_counter_ns()) / 1e9)
                try:
                    code = process.wait(timeout=remaining)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    kill_group()
                    code = process.wait()
        except BaseException as error:
            failure = type(error).__name__
            _termination = failure
            if process is not None:
                kill_group()
                code = process.wait()
        finally:
            if process is not None:
                if process.poll() is None:
                    kill_group()
                    code = process.wait()
                if process.stdout is not None:
                    # Preserve bytes already committed to the pipe after a
                    # deadline/interruption. Reads are nonblocking and bounded
                    # by actual remaining bytes; no new child is launched here.
                    while True:
                        try:
                            chunk = os.read(process.stdout.fileno(), 65536)
                        except (BlockingIOError, OSError):
                            break
                        if not chunk:
                            break
                        out.write(chunk)
                        output_length += len(chunk)
                    process.stdout.close()
                if not timed_out and not residual and failure is None:
                    try:
                        os.killpg(process.pid, 0)
                    except ProcessLookupError:
                        pass
                    except OSError as error:
                        failure = "group-probe:" + type(error).__name__
                        _termination = failure
                        residual = True
                        kill_group()
                    else:
                        residual = True
                        kill_group()
        finished = time.perf_counter_ns()
    stdout = prefix.with_suffix(".stdout")
    stderr = prefix.with_suffix(".stderr")
    valid = (code == 0 and not timed_out and not residual and failure is None
             and observed is not None and output_length == len(expected_line)
             and stdout.read_bytes() == expected_line)
    return {"command": command, "exit": code, "timedOut": timed_out,
            "enforcedTimeoutSeconds": timeout, "waitFailure": failure,
            "residualProcessGroupTerminated": residual, "status": "passed" if valid else "failed",
            "observedResultNs": observed, "observedProcessExitNs": finished - started,
            "firstValidatedResultNs": observed if valid else None,
            "launchToExitNs": finished - started if valid else None,
            "scope": "parent-observed complete validated stdout line; launch and transport included",
            "stdout": stdout.name, "stderr": stderr.name,
            "stdoutSha256": digest(stdout), "stderrSha256": digest(stderr)}
