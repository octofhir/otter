#!/usr/bin/env python3
"""Make the heap parameter of every function a HeapView reaches generic.
Runs `cargo check -p otter-vm --release` (JSON), and for each mismatched
`&GcHeap` argument whose callee parameter is declared `&GcHeap`, rewrites
that parameter to `&impl crate::heap_read::HeapRead`. Repeats until no such
error remains; prints the other errors' locations."""
import json, re, subprocess, sys
ROOT = '/Users/alexanderstreltsov/work/octofhir/otter'
for round_ in range(40):
    out = subprocess.run(['cargo', 'check', '-p', 'otter-vm', '--release', '--message-format=json'],
                         cwd=ROOT, capture_output=True, text=True).stdout
    edits = set()
    other = []
    for line in out.splitlines():
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get('reason') != 'compiler-message' or m['message']['level'] != 'error':
            continue
        msg = m['message']
        text = msg['rendered']
        if 'GcHeap' in text and ('HeapView' in text or 'impl HeapRead' in text or 'HeapRead' in text):
            for ch in msg.get('children', []):
                if 'defined here' not in ch['message']:
                    continue
                for sp in ch['spans']:
                    t = sp['text'][0]['text'] if sp['text'] else ''
                    if 'GcHeap' in t:
                        edits.add((sp['file_name'], sp['line_start']))
        else:
            if msg['spans']:
                sp = msg['spans'][0]
                other.append(f"{sp['file_name']}:{sp['line_start']}: {msg['message']}")
    if not edits:
        print('\n'.join(sorted(set(other))[:60]))
        print('rounds', round_, 'remaining other', len(set(other)))
        break
    for f, l in sorted(edits):
        p = f'{ROOT}/{f}'
        lines = open(p).read().split('\n')
        old = lines[l - 1]
        new = re.sub(r'&(mut )?(otter_gc::)?GcHeap\b', lambda mm: '&impl crate::heap_read::HeapRead' if not mm.group(1) else mm.group(0), old, count=1)
        if new == old:
            print('could not rewrite', f, l, old.strip())
        lines[l - 1] = new
        open(p, 'w').write('\n'.join(lines))
    print('round', round_, 'rewrote', len(edits))
