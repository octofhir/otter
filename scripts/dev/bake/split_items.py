#!/usr/bin/env python3
"""List the 4-space-indented items (doc + fn) of one impl region of a Rust file.
usage: split_items.py <file> <start> <end>   (1-based inclusive line range)
Prints: <first line> <last line> <fn name>"""
import re, sys
path, start, end = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = open(path).read().split('\n')
i = start - 1
item_start = None
while i < end:
    line = lines[i]
    if item_start is None and (line.startswith('    ///') or line.startswith('    #[') or line.startswith('    //')):
        item_start = i
    m = re.match(r'^    (pub(\([a-z]+\))? )?(unsafe )?fn (\w+)', line)
    if m:
        s = item_start if item_start is not None else i
        j = i
        while lines[j] != '    }':
            j += 1
        print(s + 1, j + 1, m.group(4))
        item_start = None
        i = j + 1
        continue
    if line.strip() == '' :
        item_start = None if item_start is None else item_start
    i += 1
