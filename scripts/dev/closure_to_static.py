#!/usr/bin/env python3
"""Rewrite `fn NAME(captures) -> NativeCall { let call: Arc<NativeFn> =
Arc::new(move |ctx, args, _captures| { BODY }); NativeCall::Dynamic(call) }`
into a static native `fn NEW(ctx: &mut NativeCtx<'_>, args: &[Value]) ->
Result<Value, NativeError> { PRELUDE BODY }` with BODY dedented one level.

usage: closure_to_static.py <file> <old fn name> <new fn name> <prelude line>...
"""
import re, sys
path, old, new, *prelude = sys.argv[1:]
s = open(path).read()
m = re.search(r'\nfn ' + re.escape(old) + r'\([^)]*\) -> NativeCall \{\n', s)
assert m, old
head_start = m.start() + 1
closure = re.compile(r'    let call: (?:std::sync::)?Arc<NativeFn> = (?:std::sync::)?Arc::new\(move \|_?ctx, (args|_args), _captures\| \{\n')
c = closure.match(s, m.end())
assert c, 'closure line after ' + old
args_name = c.group(1)
tail = '    });\n    NativeCall::Dynamic(call)\n}\n'
end = s.index(tail, c.end())
body = s[c.end():end]
body = '\n'.join(line[4:] if line.startswith('    ') else line for line in body.split('\n'))
pre = ''.join('    ' + line + '\n' for line in prelude)
new_fn = f"fn {new}(ctx: &mut NativeCtx<'_>, {args_name}: &[Value]) -> Result<Value, NativeError> {{\n{pre}{body}}}\n"
s = s[:head_start] + new_fn + s[end + len(tail):]
open(path, 'w').write(s)
