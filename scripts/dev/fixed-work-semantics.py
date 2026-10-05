#!/usr/bin/env python3
"""Validate all seven original and persistent Scripts before competitive capture.

Every original runs with the same classic-Script host prefix as its generated
counterpart. Independent original outputs precede freezing the crypto oracle.
Generated clock observations are semantic diagnostics here, never speed scores.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
WARM_DRIVER = ROOT / "scripts/dev/fixed-work-warm.py"
ENGINES = ("otter-interpreter", "otter", "bun", "node")
ORIGINAL_STDOUT = {
    "fib": b"4160200\n", "mega_method": b"1396134912\n", "ast_ctor": b"100021998720\n",
    "ts": b"", "zlib": b"zlib-ok:720:72000000:2773014\n", "earley-boyer": b"",
}


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    warm = load_module("fixed_work_warm", WARM_DRIVER)
    helpers = load_module("fixed_work_helpers", warm.HELPERS)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--prepared", required=True, type=Path)
    parser.add_argument("--matrix", required=True, type=Path)
    parser.add_argument("--otter", required=True, type=Path)
    parser.add_argument("--interpreter", required=True, type=Path,
                        help="Frozen checked Otter; invoked with --interpreter")
    parser.add_argument("--validator", required=True, type=Path)
    parser.add_argument("--build-manifest", required=True, type=Path)
    parser.add_argument("--bun", type=Path, default=shutil.which("bun"))
    parser.add_argument("--node", type=Path, default=shutil.which("node"))
    parser.add_argument("--timeout", type=warm.positive, default=1800)
    args = parser.parse_args()
    warm.install_termination_handler()
    args.out.mkdir(parents=True, exist_ok=False)
    source = helpers.source_provenance()
    metadata = {"purpose": "untimed original/generated semantic validation", "status": "running",
                "source": source, "platform": platform.platform(), "executables": {},
                "prepared": {}, "files": {}, "failures": [], "rows": [],
                "samplingUse": "all generated warm/measured records checked; durations are not scores",
                "oracle": "analytic checksums or independent complete original outputs; no speed sample"}
    for phase in ("original", "generated"):
        for anchor in warm.ANCHORS:
            for engine in ENGINES:
                metadata["rows"].append({"phase": phase, "anchor": anchor, "engine": engine,
                                         "status": "unrun", "failures": []})

    def save():
        warm.save_json(args.out / "semantics.json", metadata)

    def failed(error):
        metadata["failures"].append(str(error))
        for row in metadata["rows"]:
            row["failures"].append(str(error))
            row["status"] = "failed"
        metadata["status"] = "failed"
        save()
        return 1

    try:
        arch = warm.canonical_arch(platform.machine())
        metadata["nativeArchitecture"] = arch
        metadata["prepared"], metadata["files"] = warm.prepared_bundle(
            args.prepared.resolve(), args.matrix, require_oracle=False)
        for key, path in (("harness", __file__), ("warmDriver", WARM_DRIVER), ("helpers", warm.HELPERS),
                          ("loader", warm.LOADER), ("validator", args.validator),
                          ("buildManifest", args.build_manifest)):
            metadata["files"][key] = warm.file_identity(path)
        warm.verify_build(args.build_manifest, source, {
            "otter": (args.otter, "release"), "otter-checked": (args.interpreter, "checked"),
            "otter-warm-harness": (args.validator, "release")})
        if warm.active_compilers():
            raise ValueError("Cargo/rustc is active; no semantic engine execution started")
        warm.reject_diagnostics()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        return failed("preflight: " + str(error))

    supplied = {"otter-interpreter": args.interpreter, "otter": args.otter,
                "bun": args.bun, "node": args.node}
    for engine, path in supplied.items():
        probe = None
        try:
            warm.check_capture_active()
            if path is None:
                raise ValueError("engine executable is unavailable")
            identity = warm.file_identity(path)
            identity["architecture"] = warm.native_binary(identity["path"], arch)
            version = warm.capture([identity["path"], "--version"], args.out / (engine + "-identity"), 15)
            probe = version
            if version["exit"] or version["timedOut"] or version["waitFailure"] or version["residualProcessGroupTerminated"]:
                raise ValueError("version probe failed")
            identity.update(version=(args.out / version["stdout"]).read_text().strip(), identityProbe=version)
            metadata["executables"][engine] = identity
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            metadata["executables"][engine] = {"status": "unavailable", "failure": str(error), "identityProbe": probe}
            for row in metadata["rows"]:
                if row["engine"] == engine:
                    row.update(status="failed", failures=["unavailable native engine: " + str(error)])
    save()
    try:
        warm.check_capture_active()
    except ValueError as error:
        return failed(error)

    def verify_identities():
        warm.check_capture_active()
        if helpers.source_provenance()["treeSha256"] != source["treeSha256"]:
            raise ValueError("source changed during semantic validation")
        warm.verify_files(metadata["files"])
        for identity in metadata["executables"].values():
            if "path" in identity and warm.digest(identity["path"]) != identity["sha256"]:
                raise ValueError("engine executable changed during semantic validation")
        if warm.active_compilers():
            raise ValueError("Cargo/rustc is active during semantic validation")

    def execute_phase(phase):
        for row in metadata["rows"]:
            if row["phase"] != phase or row["status"] != "unrun":
                continue
            anchor, engine = row["anchor"], row["engine"]
            case = metadata["prepared"][anchor]
            prefix = args.out / (anchor + "-" + engine + "-" + phase)
            try:
                verify_identities()
                path = case["originalScript"] if phase == "original" else case["script"]
                executable = metadata["executables"][engine]["path"]
                command = ([executable, "--interpreter", "run", path] if engine == "otter-interpreter"
                           else [executable, "run", path] if engine == "otter"
                           else [executable, str(warm.LOADER), path])
                print("START", phase, anchor, engine, flush=True)
                row.update(warm.capture(command, prefix, args.timeout))
                if row["waitFailure"]:
                    raise ValueError("capture wait failed: " + row["waitFailure"])
                if row["exit"] or row["timedOut"] or row["residualProcessGroupTerminated"]:
                    raise ValueError("execution failed or reached its process-group deadline")
                output = prefix.with_suffix(".stdout").read_bytes()
                if phase == "original":
                    if anchor == "crypto":
                        text = output.decode("ascii")
                        if (not text.endswith("\n") or not text[:-1] or len(text[:-1]) > 256
                                or len(text[:-1]) % 2 or any(c not in "0123456789abcdef" for c in text[:-1])):
                            raise ValueError("original crypto output is not one complete ciphertext")
                    elif output != ORIGINAL_STDOUT[anchor]:
                        raise ValueError("original output differs from the complete fixed-work contract")
                else:
                    validated = prefix.with_suffix(".validated.json")
                    validation = warm.capture([str(args.validator.resolve()), "validate", "--manifest", case["manifest"],
                                               "--stdout", str(prefix.with_suffix(".stdout")), "--out", str(validated)],
                                              prefix.with_name(prefix.name + "-validation"), 30)
                    row["validation"] = validation
                    if (validation["exit"] or validation["timedOut"] or validation["waitFailure"] or validation["residualProcessGroupTerminated"]
                            or not validated.is_file()):
                        raise ValueError("strict generated semantic/protocol validation failed")
                    observations = json.loads(validated.read_text())
                    if observations["anchor"] != anchor or len(observations["warmups"]) < 3 or len(observations["samples"]) < 5:
                        raise ValueError("missing complete persistent invocation observations")
                    row.update(validatedFile=validated.name, validatedSha256=warm.digest(validated))
                row["status"] = "passed"
            except (OSError, ValueError, KeyError, TypeError, UnicodeError, subprocess.SubprocessError) as error:
                row["status"] = "failed"
                row["failures"].append(str(error))
                if ("changed" in str(error) or "Cargo/rustc" in str(error)
                        or "capture wait failed" in str(error) or warm._capture_termination is not None):
                    metadata["failures"].append(str(error))
                    save()
                    return False
            save()
            print("END", phase, anchor, engine, row["status"], flush=True)
        return True

    if not execute_phase("original"):
        return failed("original phase lost frozen identity")
    crypto_rows = [row for row in metadata["rows"] if row["phase"] == "original" and row["anchor"] == "crypto"]
    try:
        if len(crypto_rows) != 4 or any(row["status"] != "passed" for row in crypto_rows):
            raise ValueError("crypto oracle requires all four independent original runs")
        outputs = [(args.out / row["stdout"]).read_bytes() for row in crypto_rows]
        if any(output != outputs[0] for output in outputs[1:]):
            raise ValueError("independent original crypto outputs disagree")
        oracle = {"kind": "text", "value": outputs[0].decode("ascii").removesuffix("\n")}
        oracle_path = args.out / "crypto-original-oracle.json"
        warm.save_json(oracle_path, oracle)
        case = metadata["prepared"]["crypto"]
        manifest = json.loads(Path(case["manifest"]).read_text())
        if manifest["expectedResult"] is None:
            verify_identities()
            freeze = warm.capture([str(args.validator.resolve()), "freeze", "--manifest", case["manifest"],
                                   "--expected-result", str(oracle_path)], args.out / "crypto-oracle-freeze", 30)
            metadata["oracleFreeze"] = freeze
            if freeze["exit"] or freeze["timedOut"] or freeze["waitFailure"] or freeze["residualProcessGroupTerminated"]:
                raise ValueError("crypto oracle freeze failed")
        expected_manifest = dict(manifest, expectedResult=oracle)
        if json.loads(Path(case["manifest"]).read_text()) != expected_manifest:
            raise ValueError("crypto freeze changed fields beyond the agreed original oracle")
        unchanged = {key: value for key, value in metadata["files"].items() if key != "crypto:manifest"}
        warm.verify_files(unchanged)
        metadata["oracleEvidence"] = {"file": oracle_path.name, "sha256": warm.digest(oracle_path),
            "engines": list(ENGINES), "originalStdoutSha256": crypto_rows[0]["stdoutSha256"],
            "manifestBeforeSha256": case["manifestSha256"]}
        # Preserve every earlier identity. Only the verified oracle-binding
        # write changes one manifest; refreshing the whole bundle would hide
        # a concurrent edit to the scripts already used by original runs.
        new_manifest = warm.file_identity(case["manifest"])
        metadata["files"]["crypto:manifest"] = new_manifest
        case["manifestSha256"] = new_manifest["sha256"]
        save()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        metadata["failures"].append(str(error))
        save()

    if not execute_phase("generated"):
        return failed("generated phase lost frozen identity")
    try:
        verify_identities()
        metadata["sourceAfter"] = helpers.source_provenance()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return failed(error)
    metadata["status"] = ("passed" if not metadata["failures"] and all(row["status"] == "passed" for row in metadata["rows"])
                          else "failed")
    save()
    return 0 if metadata["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
