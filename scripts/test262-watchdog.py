#!/usr/bin/env python3
"""Bound one serial Test262 batch and clean up its complete process group.

The runner owns per-test limits; this outer finite watchdog also covers a stuck
supervisor. Expiry exits124, preventing a missing batch from being published.
"""
import argparse
import os
import signal
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if args.seconds <= 0 or not command:
        parser.error("positive --seconds and a command are required")
    process = subprocess.Popen(command, start_new_session=True)
    try:
        try:
            status = process.wait(timeout=args.seconds)
            return status if status >= 0 else 128 - status
        except subprocess.TimeoutExpired:
            print(f"Test262 batch watchdog expired after {args.seconds}s", file=sys.stderr)
            return 124
        except KeyboardInterrupt:
            return 130
    finally:
        # Children may outlive a crashed supervisor. Their process group belongs
        # only to this batch, including on timeout and operator interruption.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


if __name__ == "__main__":
    sys.exit(main())
