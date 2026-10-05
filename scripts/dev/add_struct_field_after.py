#!/usr/bin/env python3
"""Insert `<field>: <value>,` into struct literals reported by rustc.

usage: cargo check ... 2>&1 | add_struct_field_after.py <anchor_field> <new_field> <value>

Reads `--> path:line` locations of "missing field" errors from stdin; for each,
finds the first `<anchor_field>:` line at or after the literal start and
inserts the new field after it with the same indentation.
"""
import re, sys
anchor, field, value = sys.argv[1:4]
sites = {}
for line in sys.stdin:
    m = re.search(r"--> ([^:]+):(\d+):", line)
    if m:
        sites.setdefault(m.group(1), set()).add(int(m.group(2)))
for path, lines in sites.items():
    text = open(path).read().split("\n")
    for start in sorted(lines, reverse=True):
        for i in range(start - 1, min(start + 200, len(text))):
            m = re.match(rf"^(\s*){anchor}(:|,)", text[i])
            if m:
                text.insert(i + 1, f"{m.group(1)}{field}: {value},")
                break
        else:
            print(f"no anchor after {path}:{start}", file=sys.stderr)
    open(path, "w").write("\n".join(text))
    print(f"{path}: {len(lines)}")
