#!/usr/bin/env python3
"""Capture all seven precompiled persistent workloads in matched process blocks.

Preparation and oracle freezing precede this command. Source parsing, setup,
reset, JSON output and checks outside the original algorithms are excluded by
the prepared embedded clock. Process elapsed time is diagnostic only. Every
process retains its three warmups and at least five measured invocations;
block medians, rather than dependent invocations, establish empirical spread.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
ANCHORS = ("fib", "mega_method", "ast_ctor", "ts", "zlib", "crypto", "earley-boyer")
LOADER = ROOT / "scripts/dev/fixed-work-script-loader.cjs"
HELPERS = ROOT / "scripts/dev/fixed-work.py"
_capture_termination = None


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def positive(value):
    value = float(value)
    if not 0 < value < float("inf"):
        raise argparse.ArgumentTypeError("expected a finite positive number")
    return value


def reject_diagnostics():
    # Several production diagnostics test presence, so even "0" or an empty
    # value can enable them. A common competitive environment has no overrides.
    enabled = sorted(key for key in os.environ if key.startswith("OTTER_"))
    if enabled:
        raise ValueError("remove Otter diagnostic/stress environment before capture: " + ", ".join(enabled))


def install_termination_handler():
    def terminate(signum, _frame):
        raise SystemExit(128 + signum)

    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, terminate)


def check_capture_active():
    if _capture_termination is not None:
        raise ValueError("capture terminated: " + _capture_termination)


def fresh_relative(directory, name):
    path = (directory / name).resolve()
    if path.parent != directory.resolve() or not path.is_file():
        raise ValueError("prepared file must be an existing direct child: " + name)
    return path


def file_identity(path):
    path = Path(path).resolve(strict=True)
    return {"path": str(path), "sha256": digest(path)}


def verify_files(files):
    for identity in files.values():
        if digest(identity["path"]) != identity["sha256"]:
            raise ValueError("captured file changed: " + identity["path"])


def prepared_bundle(directory, matrix_path, require_oracle=True):
    """Join every emitted source to the predeclared complete original anchor."""
    index_path = directory / "prepared.json"
    matrix = json.loads(Path(matrix_path).read_text())
    entries = json.loads(index_path.read_text())["anchors"]
    if len(entries) != 7 or {entry["anchor"] for entry in entries} != set(ANCHORS):
        raise ValueError("prepared manifest must contain exactly the seven pinned anchors")
    files = {"preparedIndex": file_identity(index_path), "matrix": file_identity(matrix_path)}
    cases = {}
    for entry in entries:
        anchor = entry["anchor"]
        script = fresh_relative(directory, entry["script"])
        original = fresh_relative(directory, entry["originalScript"])
        manifest_path = fresh_relative(directory, entry["manifest"])
        fixture = ROOT / "benchmarks/fixtures/fixed-work" / (anchor + ".js")
        manifest = json.loads(manifest_path.read_text())
        declared = matrix["anchors"][anchor]
        if (manifest["anchor"] != anchor or manifest["generatedSha256"] != digest(script)
                or manifest["originalScriptSha256"] != digest(original)
                or manifest["originalSha256"] != declared["sha256"]
                or digest(fixture) != declared["sha256"]
                or manifest["sourceName"] != declared["path"]):
            raise ValueError("original/generated source identity mismatch for " + anchor)
        if require_oracle and manifest.get("expectedResult") is None:
            raise ValueError("freeze the independent semantic oracle before capture: " + anchor)
        cases[anchor] = {"script": str(script), "manifest": str(manifest_path),
            "originalScript": str(original), "originalFixture": str(fixture),
            "scriptSha256": digest(script), "manifestSha256": digest(manifest_path),
            "originalScriptSha256": digest(original), "originalSha256": declared["sha256"],
            "scope": manifest["scope"]}
        for kind, path in (("script", script), ("originalScript", original),
                           ("manifest", manifest_path), ("originalFixture", fixture)):
            files[anchor + ":" + kind] = file_identity(path)
    return cases, files


def verify_build(build_path, source, binaries):
    build = json.loads(Path(build_path).read_text())
    if build["status"] != "passed":
        raise ValueError("build manifest is not complete")
    for key, (path, profile) in binaries.items():
        executable = build["executables"][key]
        if (executable["profile"] != profile or executable["sha256"] != digest(path)
                or executable["sourceTreeSha256"] != source["treeSha256"]
                or executable["debugAssertions"] != (profile == "checked")):
            raise ValueError("build does not prove this frozen source/profile/binary: " + key)


def verify_semantics(path, source, cases, executables, validator):
    path = Path(path).resolve(strict=True)
    report = json.loads(path.read_text())
    if (report["status"] != "passed" or report["purpose"] != "untimed original/generated semantic validation"
            or report["failures"]
            or report["source"]["treeSha256"] != source["treeSha256"]
            or report["sourceAfter"]["treeSha256"] != source["treeSha256"]
            or report["files"]["validator"]["sha256"] != digest(validator)):
        raise ValueError("semantic validation does not prove this frozen source and validator")
    verify_files(report["files"])

    def verify_raw(record):
        if record["exit"] or record["timedOut"] or record["waitFailure"] or record["residualProcessGroupTerminated"]:
            raise ValueError("failed raw semantic observation")
        for kind in ("stdout", "stderr"):
            raw = fresh_relative(path.parent, record[kind])
            if digest(raw) != record[kind + "Sha256"]:
                raise ValueError("raw semantic evidence changed: " + str(raw))

    for anchor, case in cases.items():
        proved = report["prepared"][anchor]
        for key in ("scriptSha256", "manifestSha256", "originalScriptSha256", "originalSha256"):
            if proved[key] != case[key]:
                raise ValueError("semantic validation used a different prepared identity: " + anchor)
        for engine in ("otter-interpreter", "otter", "bun", "node"):
            matched = [row for row in report["rows"] if row["anchor"] == anchor and row["engine"] == engine]
            if len(matched) != 2 or {row["phase"] for row in matched} != {"original", "generated"}:
                raise ValueError("missing original/generated semantic evidence: " + anchor + "/" + engine)
            if any(row["status"] != "passed" for row in matched):
                raise ValueError("failed original/generated semantic evidence: " + anchor + "/" + engine)
            for row in matched:
                verify_raw(row)
                if row["phase"] == "generated":
                    verify_raw(row["validation"])
                    validated = fresh_relative(path.parent, row["validatedFile"])
                    if digest(validated) != row["validatedSha256"]:
                        raise ValueError("validated semantic evidence changed: " + str(validated))
                    observation = json.loads(validated.read_text())
                    if (observation["anchor"] != anchor or len(observation["warmups"]) < 3
                            or len(observation["samples"]) < 5):
                        raise ValueError("incomplete validated semantic evidence: " + anchor + "/" + engine)
    for engine, executable in executables.items():
        if "sha256" in executable and report["executables"][engine]["sha256"] != executable["sha256"]:
            raise ValueError("semantic validation used a different executable: " + engine)


def capture(command, prefix, timeout):
    """Keep raw bytes and bound the complete session's process-group lifetime."""
    global _capture_termination
    check_capture_active()
    completed = threading.Event()
    lock = threading.Lock()
    timed_out = False
    residual_group = False
    wait_failure = None
    group_failure = None
    with prefix.with_suffix(".stdout").open("wb") as out, prefix.with_suffix(".stderr").open("wb") as err:
        started = time.perf_counter()
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=err, start_new_session=True)

        def kill_owned_group():
            nonlocal group_failure
            global _capture_termination
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except OSError as error:
                group_failure = "group-kill:" + type(error).__name__
                _capture_termination = group_failure
                # Reap the owned leader even if the operating system refused
                # the group operation. The sample remains invalid and the
                # capture cannot launch another group after this failure.
                try:
                    process.kill()
                except ProcessLookupError:
                    pass

        def expire():
            nonlocal timed_out
            with lock:
                if completed.is_set():
                    return
                timed_out = True
                kill_owned_group()

        watchdog = threading.Timer(max(0, timeout - (time.perf_counter() - started)), expire)
        watchdog.daemon = True
        watchdog.start()
        try:
            try:
                code = process.wait()
            except BaseException as error:
                # The private session keeps running when the caller is
                # interrupted. End and reap our child before cancelling its
                # deadline, retaining an invalid observation for the caller.
                wait_failure = type(error).__name__
                _capture_termination = wait_failure
                kill_owned_group()
                code = process.wait()
            finished = time.perf_counter()
            # A successful leader is not completion while its descendants
            # still own this session. Terminate that owned group and reject it.
            # A terminated group has already received SIGKILL. On macOS a
            # second probe of its orphaned zombie group can return EPERM;
            # probing it adds no completion evidence after leader reaping.
            if not timed_out and wait_failure is None:
                try:
                    os.killpg(process.pid, 0)
                except ProcessLookupError:
                    pass
                except OSError as error:
                    group_failure = "group-probe:" + type(error).__name__
                    _capture_termination = group_failure
                    residual_group = True
                    kill_owned_group()
                else:
                    residual_group = True
                    kill_owned_group()
        finally:
            with lock:
                completed.set()
            watchdog.cancel()
            watchdog.join()
    return {"command": command, "exit": code, "timedOut": timed_out,
            "waitFailure": wait_failure or group_failure,
            "residualProcessGroupTerminated": residual_group,
            "launchToExitSecondsDiagnostic": finished - started,
            "stdout": prefix.with_suffix(".stdout").name,
            "stderr": prefix.with_suffix(".stderr").name,
            "stdoutSha256": digest(prefix.with_suffix(".stdout")),
            "stderrSha256": digest(prefix.with_suffix(".stderr"))}


def active_compilers():
    rows = subprocess.check_output(["ps", "-axo", "pid,comm"], text=True).splitlines()[1:]
    return [row.strip() for row in rows if Path(row.split(None, 1)[-1]).name in ("cargo", "rustc")]


def idle_observation():
    """Observe whole-host idle fraction without listing unrelated app arguments."""
    if sys.platform == "darwin":
        result = subprocess.run(["top", "-l", "2", "-s", "1", "-n", "0"],
                                capture_output=True, text=True, timeout=15, check=True)
        lines = [line for line in result.stdout.splitlines() if line.startswith("CPU usage:")]
        if len(lines) != 2:
            raise ValueError("missing complete macOS interval CPU observations")
        value = lines[-1].rsplit(",", 1)[-1].strip().removesuffix(" idle")
        return {"idleFraction": float(value.removesuffix("%")) / 100,
                "raw": lines, "method": "top second interval whole-host idle"}
    if sys.platform.startswith("linux"):
        def read_cpu():
            fields = Path("/proc/stat").read_text().splitlines()[0].split()
            if fields[0] != "cpu" or len(fields) < 9:
                raise ValueError("missing Linux whole-host CPU counters")
            # Guest counters are already included in user/nice, not extra time.
            return [int(field) for field in fields[1:9]]
        before = read_cpu()
        time.sleep(1)
        after = read_cpu()
        delta = [b - a for a, b in zip(before, after)]
        total = sum(delta)
        if total <= 0 or any(value < 0 for value in delta):
            raise ValueError("invalid Linux CPU counter interval")
        return {"idleFraction": delta[3] / total, "raw": [before, after],
                "method": "/proc/stat one-second idle; iowait counts as non-idle"}
    raise ValueError("warm capture requires native macOS or Linux")


def canonical_arch(value):
    value = value.lower().replace("-", "_")
    if value in ("arm64", "aarch64"):
        return "aarch64"
    if value in ("x86_64", "amd64"):
        return "x86_64"
    raise ValueError("unsupported native architecture: " + value)


def native_binary(path, arch):
    text = subprocess.check_output(["file", "-b", str(path)], text=True).strip()
    low = text.lower()
    valid = ("arm64" in low or "aarch64" in low) if arch == "aarch64" else ("x86_64" in low or "x86-64" in low)
    if not valid:
        raise ValueError("executable does not match native host: " + text)
    return text


def summarize(row, rounds):
    good = [block for block in row["blocks"] if block["status"] == "passed"]
    values = [block["medianNs"] for block in good]
    row["blockMedianNs"] = values
    row["summary"] = ({"medianNs": statistics.median(values), "minNs": min(values),
                       "maxNs": max(values), "blocks": len(values),
                       "invocations": sum(len(block["measuredNs"]) for block in good)} if values else None)
    row["scoreable"] = len(good) == rounds and not row["failures"]
    row["status"] = "passed" if row["scoreable"] else "failed" if row["failures"] else "incomplete"


def comparison(rows, anchor):
    by_engine = {row["engine"]: row for row in rows if row["anchor"] == anchor}
    otter, bun = by_engine["otter"], by_engine["bun"]
    if not otter["scoreable"] or not bun["scoreable"]:
        return {"anchor": anchor, "status": "coverage-gap"}
    a, b = otter["summary"], bun["summary"]
    status = "win" if a["maxNs"] < b["minNs"] else "loss" if b["maxNs"] < a["minNs"] else "overlap"
    return {"anchor": anchor, "status": status, "otterOverBunMedian": a["medianNs"] / b["medianNs"],
            "otter": a, "bun": b, "spreadUnit": "independent process-block medians"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--prepared", required=True, type=Path)
    parser.add_argument("--otter", required=True, type=Path)
    parser.add_argument("--node", type=Path, default=shutil.which("node"))
    parser.add_argument("--bun", type=Path, default=shutil.which("bun"))
    parser.add_argument("--validator", required=True, type=Path)
    parser.add_argument("--build-manifest", required=True, type=Path,
                        help="Captured frozen-source release build manifest for this Otter")
    parser.add_argument("--matrix", required=True, type=Path,
                        help="Predeclared competitive matrix with immutable original hashes")
    parser.add_argument("--semantic-manifest", required=True, type=Path,
                        help="Complete untimed original/generated validation for these files/binaries")
    parser.add_argument("--rounds", type=int, choices=(5, 15, 30), default=5)
    parser.add_argument("--timeout", type=positive, default=1800)
    parser.add_argument("--max-host-busy-fraction", type=positive, default=0.10)
    args = parser.parse_args()
    install_termination_handler()
    if args.max_host_busy_fraction >= 1:
        parser.error("host busy fraction must be below one")
    args.out.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("fixed_work_helpers", HELPERS)
    helpers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helpers)
    source = helpers.source_provenance()
    prepared = args.prepared.resolve()
    rows = [{"anchor": anchor, "engine": engine, "blocks": [], "failures": [], "scoreable": False}
            for anchor in ANCHORS for engine in ("otter", "bun", "node")]
    by_key = {(row["anchor"], row["engine"]): row for row in rows}
    metadata = {"purpose": "competitive persistent fixed-work speed capture",
                "source": source, "platform": platform.platform(),
                "nativeArchitecture": platform.machine(),
                "harnessSha256": digest(__file__), "helperSha256": digest(HELPERS),
                "engineOrder": "rotated process blocks by anchor and round",
                "rounds": args.rounds, "excludedWarmupsMinimum": 3,
                "measuredInvocationsPerBlockMinimum": 5,
                "clock": "captured-process-hrtime-bigint", "scope": "prepared manifest per case",
                "spread": "independent process-block median; raw dependent samples retained",
                "idlePreflight": {"maxBusyFraction": args.max_host_busy_fraction,
                                  "intervals": 3, "timing": "immediately before each engine block"},
                "resources": "separate capture required; process elapsed is diagnostic",
                "escalation": "overlap requires fresh 15 then 30 matched process-block rounds",
                "executables": {}, "prepared": {}, "files": {}, "failures": [], "status": "running"}

    def save():
        for row in rows:
            summarize(row, args.rounds)
        metadata["comparisons"] = [comparison(rows, anchor) for anchor in ANCHORS]
        save_json(args.out / "capture.json", metadata)
        save_json(args.out / "results.json", rows)

    try:
        arch = canonical_arch(platform.machine())
        metadata["nativeArchitecture"] = arch
        metadata["prepared"], metadata["files"] = prepared_bundle(prepared, args.matrix)
        for key, path in (("harness", __file__), ("helpers", HELPERS), ("loader", LOADER),
                          ("validator", args.validator), ("buildManifest", args.build_manifest)):
            metadata["files"][key] = file_identity(path)
        verify_build(args.build_manifest, source, {"otter": (args.otter, "release"),
                                                  "otter-warm-harness": (args.validator, "release")})
        metadata["files"]["semanticManifest"] = file_identity(args.semantic_manifest)
        if active_compilers():
            raise ValueError("Cargo/rustc is active; no measurements started")
        reject_diagnostics()
    except (OSError, ValueError, KeyError, TypeError) as error:
        metadata.update(status="failed", failures=["preflight: " + str(error)])
        for row in rows:
            row["failures"].extend(metadata["failures"])
        save()
        return 1

    for engine, supplied in [("otter", args.otter), ("bun", args.bun), ("node", args.node)]:
        probe = None
        try:
            check_capture_active()
            if supplied is None:
                raise ValueError("engine executable is unavailable")
            path = Path(supplied).resolve(strict=True)
            executable = {"path": str(path), "sha256": digest(path), "architecture": native_binary(path, arch)}
            version = capture([str(path), "--version"], args.out / (engine + "-identity"), 15)
            probe = version
            if version["exit"] or version["timedOut"] or version["waitFailure"] or version["residualProcessGroupTerminated"]:
                raise ValueError("version probe failed")
            executable.update(version=(args.out / version["stdout"]).read_text().strip(), identityProbe=version)
            metadata["executables"][engine] = executable
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            metadata["executables"][engine] = {"status": "unavailable", "failure": str(error), "identityProbe": probe}
            for row in rows:
                if row["engine"] == engine:
                    row["failures"].append("unavailable native engine: " + str(error))
    save()
    if _capture_termination is not None:
        metadata.update(status="failed", failures=["identity capture terminated: " + _capture_termination])
        for row in rows:
            row["failures"].extend(metadata["failures"])
        save()
        return 1
    try:
        verify_semantics(args.semantic_manifest, source, metadata["prepared"],
                         metadata["executables"], args.validator)
    except (OSError, ValueError, KeyError, TypeError) as error:
        metadata.update(status="failed", failures=["semantic preflight: " + str(error)])
        for row in rows:
            row["failures"].extend(metadata["failures"])
        save()
        return 1
    aborted = False
    for anchor_index, anchor in enumerate(ANCHORS):
        for round_index in range(args.rounds):
            engines = ["otter", "bun", "node"]
            offset = (anchor_index + round_index) % 3
            for engine in engines[offset:] + engines[:offset]:
                row = by_key[(anchor, engine)]
                executable = metadata["executables"][engine]
                if executable.get("status") == "unavailable":
                    continue
                prefix = args.out / f"{anchor}-{engine}-{round_index + 1:04d}"
                block = {"round": round_index + 1, "status": "failed", "failures": [], "idle": []}
                try:
                    check_capture_active()
                    if helpers.source_provenance()["treeSha256"] != source["treeSha256"]:
                        raise ValueError("source changed during capture")
                    verify_files(metadata["files"])
                    case = metadata["prepared"][anchor]
                    if digest(executable["path"]) != executable["sha256"]:
                        raise ValueError("engine executable changed")
                    if active_compilers():
                        raise ValueError("Cargo/rustc is active")
                    for _ in range(3):
                        observation = idle_observation()
                        block["idle"].append(observation)
                        if not 1 - args.max_host_busy_fraction <= observation["idleFraction"] <= 1:
                            raise ValueError("host is busy; block not executed")
                    command = ([executable["path"], "run", case["script"]] if engine == "otter"
                               else [executable["path"], str(LOADER), case["script"]])
                    print(f"START {anchor} {engine} block {round_index + 1}/{args.rounds}", flush=True)
                    block.update(capture(command, prefix, args.timeout))
                    if block["waitFailure"]:
                        metadata["failures"].append("capture wait failed: " + block["waitFailure"])
                        aborted = True
                    if block["exit"] or block["timedOut"] or block["waitFailure"] or block["residualProcessGroupTerminated"]:
                        raise ValueError("execution failed or reached its process-group deadline")
                    validated = prefix.with_suffix(".validated.json")
                    validation = capture([str(args.validator.resolve()), "validate", "--manifest", case["manifest"],
                                          "--stdout", str(prefix.with_suffix(".stdout")), "--out", str(validated)],
                                         prefix.with_name(prefix.name + "-validation"), 30)
                    block["validation"] = validation
                    if validation["exit"] or validation["timedOut"] or validation["waitFailure"] or validation["residualProcessGroupTerminated"] or not validated.is_file():
                        raise ValueError("strict semantic/protocol validation failed")
                    values = json.loads(validated.read_text())
                    measured = values["measuredNs"]
                    if (values["anchor"] != anchor or len(values["warmups"]) < 3 or len(measured) < 5
                            or len(values["samples"]) != len(measured)
                            or any(type(value) is not int or not 0 < value < 2**64 for value in measured)):
                        raise ValueError("validator output is incomplete")
                    block.update(status="passed", measuredNs=measured,
                                 medianNs=statistics.median(measured),
                                 validatedFile=validated.name, validatedSha256=digest(validated))
                except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
                    block["failures"].append(str(error))
                    row["failures"].append(f"block {round_index + 1}: {error}")
                    if "changed" in str(error) or _capture_termination is not None:
                        metadata["failures"].append(str(error))
                        aborted = True
                row["blocks"].append(block)
                save()
                print(f"END {anchor} {engine} block {round_index + 1} {block['status']}", flush=True)
                if aborted:
                    break
            if aborted:
                break
        if aborted:
            break
    try:
        metadata["sourceAfter"] = helpers.source_provenance()
        if metadata["sourceAfter"]["treeSha256"] != source["treeSha256"]:
            raise ValueError("source changed before final identity check")
        for executable in metadata["executables"].values():
            if "path" in executable and digest(executable["path"]) != executable["sha256"]:
                raise ValueError("engine binary changed before final identity check")
        verify_files(metadata["files"])
    except (OSError, ValueError) as error:
        metadata["failures"].append(str(error))
    if metadata["failures"]:
        for row in rows:
            row["failures"].extend(metadata["failures"])
    save()
    metadata["status"] = "passed" if all(row["scoreable"] for row in rows) else "failed"
    metadata["perCaseVictory"] = all(item["status"] == "win" for item in metadata["comparisons"])
    save()
    return 0 if metadata["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
