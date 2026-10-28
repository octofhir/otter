#!/usr/bin/env python3
"""Rewrite accesses of an Interpreter field moved into CompileShared.
usage: share_field.py <field> <files...>
`<recv>.<field>.<method>(` becomes `<recv>.<field>().<method>(` for reads and
`<recv>.<field>_mut().<method>(` for mutating methods; `&mut <recv>.<field>`
becomes `&mut *<recv>.<field>_mut()`."""
import re, sys
field, files = sys.argv[1], sys.argv[2:]
MUT = {'insert', 'remove', 'retain', 'entry', 'get_mut', 'clear', 'extend', 'iter_mut',
       'values_mut', 'push', 'pop', 'truncate', 'drain', 'or_insert', 'record', 'note',
       'set', 'swap_remove', 'append', 'range_mut', 'last_mut', 'first_mut', 'sort', 'dedup'}
recv = r'(\b(?:self|vm|interp|interpreter|this|runtime|state)(?:\s*\.\s*(?:vm|interp))?)'
total = 0
for p in files:
    s = open(p).read()
    def rep(m):
        global total
        method = m.group(4)
        total += 1
        suffix = '_mut()' if method in MUT else '()'
        return f'{m.group(1)}{m.group(2)}.{field}{suffix}{m.group(3)}.{method}'
    s2 = re.sub(recv + r'(\s*)\.' + field + r'\b(\s*)\.\s*([a-z_]+)', rep, s)
    s2 = re.sub(r'&mut ' + recv + r'\.' + field + r'\b(?!\()', lambda m: f'&mut *{m.group(1)}.{field}_mut()', s2)
    if s2 != s:
        open(p, 'w').write(s2)
print(field, total)
