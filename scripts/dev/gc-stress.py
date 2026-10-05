#!/usr/bin/env python3
"""GC-stress a difftest corpus subset in every execution tier.

usage: gc-stress.py <otter-binary> <out-dir> <corpus-name>... [--strides 1-16]
       gc-stress.py <otter-binary> <out-dir> --all [--repeat 100]

Each corpus file runs under OTTER_GC_STRESS=<stride> and OTTER_GC_VERIFY=1 in
the interpreter, Template (--jitless) and production tiers. Console output and
printed completion must match an unstressed interpreter run exactly, and every
process must exit 0. Results, commands and hashes land in
<out-dir>/results.json; every stdout/stderr is kept. Runs are strictly serial.
"""
import argparse, hashlib, json, os, pathlib, subprocess, sys, time

ROOT = pathlib.Path(__file__).resolve().parents[2]
TIERS = [("interpreter", ["--interpreter"]), ("template", ["--jitless"]), ("production", [])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("otter", type=pathlib.Path)
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("corpus", nargs="*")
    parser.add_argument("--all", action="store_true", help="Run every difftest corpus file")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--strides", default="1-16")
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    try:
        low, high = (int(x) for x in args.strides.split("-"))
    except ValueError:
        parser.error("--strides must be a range such as 1-16")
    if not 1 <= low <= high <= 16 or args.repeat < 1 or args.timeout < 1:
        parser.error("strides must be within 1-16; repeat and timeout must be positive")
    if args.all == bool(args.corpus):
        parser.error("choose corpus names or --all")
    corpus_dir = ROOT / "crates/otter-difftest/corpus"
    names = sorted(p.stem for p in corpus_dir.glob("*.js")) if args.all else args.corpus
    if not names:
        parser.error("the corpus is empty")
    for name in names:
        if pathlib.Path(name).name != name or not (corpus_dir / f"{name}.js").is_file():
            parser.error(f"unknown corpus name: {name}")
    exe = args.otter.resolve()
    if not exe.is_file() or not os.access(exe, os.X_OK):
        parser.error(f"not an executable file: {exe}")
    args.out.mkdir(parents=True, exist_ok=False)
    report = {"executableSha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
              "repeat": args.repeat, "strides": [low, high], "status": "running",
              "oracle": "unstressed interpreter, console and completion",
              "sources": {}, "runs": []}
    report_path = args.out / "results.json"
    def save():
        report_path.write_text(json.dumps(report, indent=2) + "\n")
    save()
    failures = 0
    for name in names:
        source = corpus_dir / f"{name}.js"
        report["sources"][name] = hashlib.sha256(source.read_bytes()).hexdigest()
        code_source = source.read_bytes().decode("utf-8")
        source_argument = f"{name}.source.js"
        (args.out / source_argument).write_bytes(code_source.encode("utf-8"))
        oracle_command = [str(exe), "--interpreter", "--timeout", "0", "-p", code_source]
        oracle_env = dict(os.environ)
        oracle_env.pop("OTTER_GC_STRESS", None)
        oracle_env.pop("OTTER_GC_VERIFY", None)
        try:
            oracle = subprocess.run(oracle_command, env=oracle_env,
                                    capture_output=True, check=True, timeout=args.timeout)
            expected = oracle.stdout
        except (subprocess.SubprocessError, OSError) as error:
            report["status"] = "failed"
            report["oracleError"] = {"source": name, "error": str(error)}
            save()
            raise SystemExit(f"Oracle failed: {name}: {error}")
        (args.out / f"{name}-oracle.stdout").write_bytes(expected)
        (args.out / f"{name}-oracle.stderr").write_bytes(oracle.stderr)
        passed = 0
        for repetition in range(args.repeat):
            for stride in range(low, high + 1):
                for tier, flags in TIERS:
                    command = [str(exe), *flags, "--timeout", "0", "-p", code_source]
                    env = dict(os.environ, OTTER_GC_STRESS=str(stride), OTTER_GC_VERIFY="1")
                    started = time.monotonic()
                    try:
                        result = subprocess.run(command, env=env, capture_output=True,
                                                timeout=args.timeout)
                        code, out, err = result.returncode, result.stdout, result.stderr
                    except subprocess.TimeoutExpired as timeout:
                        code, out, err = "timeout", timeout.stdout or b"", timeout.stderr or b""
                    except OSError as error:
                        code, out, err = "spawn-error", b"", str(error).encode("utf-8")
                    prefix = args.out / f"{name}-{stride}-{tier}-{repetition}"
                    prefix.with_suffix(".stdout").write_bytes(out)
                    prefix.with_suffix(".stderr").write_bytes(err)
                    ok = code == 0 and out == expected
                    passed += ok
                    failures += not ok
                    report["runs"].append({"source": name, "stride": stride, "tier": tier,
                                           "repetition": repetition,
                                           "commandPrefix": command[:-1],
                                           "sourceArgumentFile": source_argument, "exit": code,
                                           "matchesInterpreter": out == expected,
                                           "stdoutSha256": hashlib.sha256(out).hexdigest(),
                                           "elapsedSeconds": time.monotonic() - started})
                    with (args.out / "runs.jsonl").open("a") as journal:
                        journal.write(json.dumps(report["runs"][-1]) + "\n")
                    if not ok:
                        print(f"FAIL {name} stride={stride} tier={tier} exit={code}", flush=True)
        save()
        print(f"{name} {passed}/{3 * (high - low + 1) * args.repeat}", flush=True)
    changed = []
    if hashlib.sha256(exe.read_bytes()).hexdigest() != report["executableSha256"]:
        changed.append("executable")
    for name, original in report["sources"].items():
        if hashlib.sha256((corpus_dir / f"{name}.js").read_bytes()).hexdigest() != original:
            changed.append(name)
    if changed:
        report["changedInputs"] = changed
        failures += 1
    report["status"] = "failed" if failures else "passed"
    report["failures"] = failures
    save()
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
