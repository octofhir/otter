#!/usr/bin/env python3
"""Inside every `impl Baker` / `impl<'a> Baker<'a>` block of the given files:
make methods take `&mut self` and route every `self.<name>` that is not a
Baker method or field through `self.vm.<name>`."""
import re, sys
files = sys.argv[1:]
own = {'vm', 'roots', 'install', 'events', 'found_shapes'}
blocks = []
for p in files:
    s = open(p).read()
    for m in re.finditer(r"^impl(<'a>)? (super::)?Baker<'(_|a)> \{\n", s, re.M):
        start = m.end()
        end = s.index('\n}\n', start)
        blocks.append((p, m.start(), start, end))
        for f in re.finditer(r"^    (?:pub(?:\([a-z]+\))? )?fn (\w+)", s[start:end], re.M):
            own.add(f.group(1))
for p in files:
    s = open(p).read()
    out, pos = [], 0
    for m in re.finditer(r"^impl(<'a>)? (super::)?Baker<'(_|a)> \{\n", s, re.M):
        start = m.end()
        end = s.index('\n}\n', start)
        body = s[start:end]
        body = re.sub(r"\(\s*&self\b", "(&mut self", body)
        body = re.sub(r"(fn \w+(?:<[^>]*>)?\(\s*)&self\b", r"\1&mut self", body)
        body = re.sub(r"\bself\.vm(\s*)\.([a-z_][a-z0-9_]*)", lambda k: 'self' + k.group(1) + '.' + k.group(2) if k.group(2) in own else k.group(0), body)
        body = re.sub(r"\bself(\s*)\.([a-z_][a-z0-9_]*)", lambda k: k.group(0) if k.group(2) in own else 'self' + k.group(1) + '.vm.' + k.group(2), body)
        body = body.replace('&mut self.vm.', '&self.vm.')
        out.append(s[pos:start]); out.append(body); pos = end
    out.append(s[pos:])
    open(p, 'w').write(''.join(out))
print(sorted(own))
