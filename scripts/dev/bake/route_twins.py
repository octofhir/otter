#!/usr/bin/env python3
"""Route generic-heap call sites to the `_in` twins of public GcHeap functions.
Repeats cargo check; at each `expected &GcHeap, found &impl HeapRead / HeapView`
call whose callee has a `<name>_in` twin, rewrites the call name."""
import json, re, subprocess
ROOT = '/Users/alexanderstreltsov/work/octofhir/otter'
TWINS = {'state','get','lookup_own','lookup','prototype','prototype_value','eq_str','to_utf16_vec','context','own_props','as_closure','is_vm_intrinsic','jit_static_call_ref'}
for r in range(20):
    out = subprocess.run(['cargo','check','-p','otter-vm','--release','--tests','--message-format=json'], cwd=ROOT, capture_output=True, text=True).stdout
    edits = []
    rest = []
    for line in out.splitlines():
        try: m = json.loads(line)
        except ValueError: continue
        if m.get('reason') != 'compiler-message' or m['message']['level'] != 'error': continue
        msg = m['message']
        if 'GcHeap' in msg['rendered'] and ('HeapRead' in msg['rendered'] or 'HeapView' in msg['rendered']):
            sp = msg['spans'][0]
            # callee name is in the secondary span of the call ("arguments to this ... are incorrect")
            callee = None
            for s2 in msg['spans']:
                if s2.get('label') and 'arguments to this' in s2['label']:
                    callee = s2
            if callee:
                edits.append((callee['file_name'], callee['line_start'], callee['column_start'], callee['column_end']))
        else:
            sp = msg['spans'][0] if msg['spans'] else None
            if sp: rest.append(f"{sp['file_name']}:{sp['line_start']}: {msg['message']}")
    if not edits:
        print('\n'.join(sorted(set(rest))[:40])); print('round', r, 'rest', len(set(rest))); break
    done = 0
    for f, l, c0, c1 in sorted(set(edits), key=lambda e: (e[0], e[1], -e[2])):
        p = f'{ROOT}/{f}'
        lines = open(p).read().split('\n')
        line = lines[l-1]
        seg = line[c0-1:c1-1]
        m = re.search(r'(\w+)$', seg)
        if not m or m.group(1) not in TWINS:
            print('skip', f, l, seg); continue
        start = c0 - 1 + m.start(1); end = c0 - 1 + m.end(1)
        lines[l-1] = line[:start] + m.group(1) + '_in' + line[end:]
        open(p,'w').write('\n'.join(lines)); done += 1
    print('round', r, 'rewrote', done)
