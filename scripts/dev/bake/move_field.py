#!/usr/bin/env python3
"""Move Interpreter fields (doc + declaration) from lib.rs into CompileShared
as MutatorOwned values, drop their initializers, and print the init lines.
usage: move_field.py <field>..."""
import re, sys
LIB = 'crates/otter-vm/src/lib.rs'
SHARED = 'crates/otter-vm/src/compile_shared.rs'
INITS = ['crates/otter-vm/src/interp/init.rs', 'crates/otter-vm/src/interp/restore.rs']
lib = open(LIB).read().split('\n')
start = next(i for i, l in enumerate(lib) if l.startswith('pub struct Interpreter {'))
end = next(i for i in range(start, len(lib)) if lib[i] == '}')
shared = open(SHARED).read()
accessors = []
for field in sys.argv[1:]:
    k = next(i for i in range(start, end) if re.match(r'^    (pub\(crate\) )?' + field + r':', lib[i]))
    d = k
    while lib[d - 1].startswith('    ///') or lib[d - 1].startswith('    #['):
        d -= 1
    e = k
    depth = 0
    while True:
        depth += lib[e].count('<') - lib[e].count('>') + lib[e].count('(') - lib[e].count(')')
        if lib[e].rstrip().endswith(',') and depth <= 0:
            break
        e += 1
    block = lib[d:e + 1]
    decl = '\n'.join(block[k - d:])
    ty = re.sub(r'^\s*(pub\(crate\) )?' + field + r':\s*', '', decl, flags=re.S).rstrip().rstrip(',').strip()
    ty = re.sub(r'\s+', ' ', ty).replace('< ', '<').replace(' >', '>').replace('( ', '(').replace(', )', ')')
    docs = '\n'.join(block[:k - d])
    del lib[d:e + 1]
    end -= e + 1 - d
    si = shared.index('pub(crate) struct CompileShared {')
    sj = shared.index('\n}\n', si)
    shared = shared[:sj] + '\n' + docs + f'\n    pub(crate) {field}: MutatorOwned<{ty}>,' + shared[sj:]
    accessors.append((field, ty))
    for p in INITS:
        s = open(p).read()
        s2 = re.sub(r'\n\s*' + field + r':[^\n]*,(?=\n)', '', s, count=1)
        if s2 == s:
            print('WARN no init in', p, field)
        open(p, 'w').write(s2)
open(LIB, 'w').write('\n'.join(lib))
open(SHARED, 'w').write(shared)
for field, ty in accessors:
    print(f'    {field}, {field}_mut: {ty};')
