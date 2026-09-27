#!/usr/bin/env python3
"""GC-stress a difftest corpus subset in every execution tier.

usage: gc-stress.py <otter-binary> <out-dir> <corpus-name>... [--strides 1-16]

Each corpus file runs under OTTER_GC_STRESS=<stride> and OTTER_GC_VERIFY=1 in
the interpreter, Template (--jitless) and production tiers. stdout must match
Node exactly and the process must exit 0. Results, commands and hashes land in
<out-dir>/results.json; every stdout/stderr is kept. Runs are strictly serial.
"""
import argparse, hashlib, json, os, pathlib, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
TIERS = [("interpreter", ["--interpreter"]), ("template", ["--jitless"]), ("production", [])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("otter", type=pathlib.Path)
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("corpus", nargs="+")
    parser.add_argument("--strides", default="1-16")
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    low, high = (int(x) for x in args.strides.split("-"))
    args.out.mkdir(parents=True, exist_ok=False)
    exe = args.otter.resolve()
    report = {"executableSha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
              "sources": {}, "runs": []}
    failures = 0
    for name in args.corpus:
        source = ROOT / "crates/otter-difftest/corpus" / f"{name}.js"
        report["sources"][name] = hashlib.sha256(source.read_bytes()).hexdigest()
        expected = subprocess.check_output(["node", str(source)], text=True)
        (args.out / f"{name}-node.stdout").write_text(expected)
        passed = 0
        for stride in range(low, high + 1):
            for tier, flags in TIERS:
                command = [str(exe), *flags, "run", str(source)]
                env = dict(os.environ, OTTER_GC_STRESS=str(stride), OTTER_GC_VERIFY="1")
                try:
                    result = subprocess.run(command, env=env, capture_output=True,
                                            text=True, timeout=args.timeout)
                    code, out, err = result.returncode, result.stdout, result.stderr
                except subprocess.TimeoutExpired as timeout:
                    code, out, err = "timeout", timeout.stdout or "", timeout.stderr or ""
                    out = out.decode() if isinstance(out, bytes) else out
                    err = err.decode() if isinstance(err, bytes) else err
                prefix = args.out / f"{name}-{stride}-{tier}"
                prefix.with_suffix(".stdout").write_text(out)
                prefix.with_suffix(".stderr").write_text(err)
                ok = code == 0 and out == expected
                passed += ok
                failures += not ok
                report["runs"].append({"source": name, "stride": stride, "tier": tier,
                                       "command": command, "exit": code, "matchesNode": out == expected})
                (args.out / "results.json").write_text(json.dumps(report, indent=2) + "\n")
                if not ok:
                    print(f"FAIL {name} stride={stride} tier={tier} exit={code}", flush=True)
        print(f"{name} {passed}/{3 * (high - low + 1)}", flush=True)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
