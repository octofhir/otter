#!/usr/bin/env python3
"""Compare null-prototype creation and reads with macOS resource counters.

Usage: null-prototype.py <fresh-output-directory>
Measures retained 100k-object creation and 2M indexed-receiver reads separately.
Counts include startup, compilation and GC; these are not ns/allocation estimates.
"""
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import sys

root = pathlib.Path(__file__).resolve().parents[2]
out = pathlib.Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=False)
executables = {
    engine: str(root / 'target/release/otter') if engine == 'otter' else shutil.which(engine)
    for engine in ['otter', 'node', 'bun']
}
versions = {
    engine: subprocess.check_output([executable, '--version'], text=True).strip()
    for engine, executable in executables.items()
}
(out / 'versions.json').write_text(json.dumps(versions, indent=2) + '\n')
rows = []
for fixture in sorted((root / 'benchmarks/fixtures/null-prototype').glob('*.js')):
    expected = None
    for engine, executable in executables.items():
        command = [executable]
        if engine == 'otter':
            command += ['run']
        if engine == 'bun':
            command += ['-e', '(0,eval)(require("node:fs").readFileSync(process.argv[1],"utf8"))']
        command.append(str(fixture))
        prefix = out / (fixture.stem + '-' + engine)
        with prefix.with_suffix('.stdout').open('w') as stdout, prefix.with_suffix('.time').open('w') as stderr:
            result = subprocess.run(['/usr/bin/time', '-l', *command], stdout=stdout, stderr=stderr)
        output = prefix.with_suffix('.stdout').read_text()
        if expected is None:
            expected = output
        assert output == expected and result.returncode == 0, (fixture, engine, result.returncode, output)
        counters = prefix.with_suffix('.time').read_text()
        row = {
            'fixture': fixture.name,
            'engine': engine,
            'command': command,
            'exit': result.returncode,
            'sourceSha256': hashlib.sha256(fixture.read_bytes()).hexdigest(),
            'executableSha256': hashlib.sha256(pathlib.Path(executable).read_bytes()).hexdigest(),
        }
        for label, key in [('instructions retired', 'instructions'), ('maximum resident set size', 'rss_bytes')]:
            row[key] = int(re.search(r'^\s*(\d+)\s+' + label + r'\s*$', counters, re.M)[1])
        rows.append(row)
        (out / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
        print(fixture.name, engine, row['instructions'], row['rss_bytes'], flush=True)
