#!/usr/bin/env python3
"""Extract a JavaScript fixture embedded in a Rust integration test.

Usage: scripts/dev/fixture-js.py <test.rs> <anchor> [out.js]

`anchor` is either a `const NAME` whose raw string is the fixture, or any
text inside the wanted `r#"..."#` literal (the first literal that contains
it). The script's completion value is printed with `console.log` when the
last statement is a bare expression, so `otter -e` shows what the test
compares. Writes to `out.js` or stdout.
"""
import re
import sys

source = open(sys.argv[1]).read()
anchor = sys.argv[2]
literals = [m.group(1) for m in re.finditer(r'r#"(.*?)"#', source, re.S)]
fixture = None
const = re.search(r"const\s+" + re.escape(anchor) + r"\s*:\s*&str\s*=\s*r#\"(.*?)\"#", source, re.S)
if const:
    fixture = const.group(1)
else:
    fixture = next((literal for literal in literals if anchor in literal), None)
if fixture is None:
    sys.exit(f"no fixture literal matches {anchor!r}")
lines = fixture.rstrip().split("\n")
last = lines[-1].strip()
if last and not re.match(r"(let|const|var|function|class|for|while|if|}|//)", last) and "=" not in last.split("(")[0]:
    lines[-1] = "console.log(" + last.rstrip(";") + ");"
text = "\n".join(lines) + "\n"
if len(sys.argv) > 3:
    open(sys.argv[3], "w").write(text)
else:
    sys.stdout.write(text)
