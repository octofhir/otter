#!/usr/bin/env python3
"""Disassemble raw AArch64 code bytes (a routine dump written beside the perf
map, `/tmp/perf-<pid>.<routine>.bin`) by wrapping them in an object file.

usage: disasm_raw.py <code.bin> [out.txt]
"""
import os, subprocess, sys, tempfile

data = open(sys.argv[1], 'rb').read()
data += b'\0' * (-len(data) % 4)
words = [int.from_bytes(data[i:i + 4], 'little') for i in range(0, len(data), 4)]
with tempfile.TemporaryDirectory() as work:
    src = os.path.join(work, 'code.s')
    obj = os.path.join(work, 'code.o')
    with open(src, 'w') as f:
        f.write('.text\n.globl _code\n_code:\n')
        f.writelines(f'.long 0x{w:08x}\n' for w in words)
    subprocess.run(['clang', '-c', '-target', 'arm64-apple-macos', src, '-o', obj], check=True)
    text = subprocess.run(['objdump', '-d', '--no-show-raw-insn', obj], check=True, capture_output=True, text=True).stdout
out = sys.argv[2] if len(sys.argv) > 2 else None
if out:
    open(out, 'w').write(text)
else:
    sys.stdout.write(text)
