#!/usr/bin/env python3
"""Assemble one test262 test into a standalone script for `otter <file>`.

Usage: scripts/dev/t262-assemble.py <test path under vendor/test262/test> <out.js>

Prepends the harness the runner would load (assert.js, sta.js, the
frontmatter `includes`, and doneprintHandle.js for `flags: [async]`) and
maps `print` onto `console.log`, so a single failing test can be rerun
under `OTTER_GC_STRESS`, `--interpreter`, or a profiler without the runner.
Module and raw tests are copied without a harness.
"""
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parents[2] / "vendor" / "test262"
test = pathlib.Path(sys.argv[1])
if not test.is_absolute():
    test = root / "test" / test if not str(test).startswith("vendor/") else root.parent.parent / test
source = test.read_text()
meta = re.search(r"/\*---(.*?)---\*/", source, re.S)
meta = meta.group(1) if meta else ""
flags = re.search(r"flags:\s*\[(.*?)\]", meta)
flags = {f.strip() for f in flags.group(1).split(",")} if flags else set()
includes = re.search(r"includes:\s*\[(.*?)\]", meta, re.S)
includes = [i.strip() for i in includes.group(1).split(",") if i.strip()] if includes else []
if not includes:
    block = re.search(r"includes:\s*\n((?:\s*-\s*\S+\s*\n)+)", meta)
    if block:
        includes = re.findall(r"-\s*(\S+)", block.group(1))

parts = ["globalThis.print = (...args) => console.log(...args);\n"]
if "raw" not in flags and "module" not in flags:
    harness = ["assert.js", "sta.js"]
    if "async" in flags:
        harness.append("doneprintHandle.js")
    harness += includes
    for name in harness:
        parts.append((root / "harness" / name).read_text())
        if name == "doneprintHandle.js":
            # The CLI runs a file in the CommonJS function scope, where a
            # function declaration is not a global the helpers can see.
            parts.append("globalThis.$DONE = $DONE;\n")
    if "onlyStrict" in flags:
        parts.insert(0, '"use strict";\n')
parts.append(source)
pathlib.Path(sys.argv[2]).write_text("\n".join(parts))
