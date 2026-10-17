#!/usr/bin/env python3
"""List non-passing tests of an otter-test262 report: path | kind | detail."""
import json, sys
d = json.load(open(sys.argv[1]))
for t in d["tests"]:
    o = t["outcome"]
    if o.get("kind") in ("pass", "skip", "skipped"):
        continue
    detail = {k: v for k, v in o.items() if k != "kind"}
    print(t["path"], "|", o.get("kind"), "|", json.dumps(detail)[:300])
