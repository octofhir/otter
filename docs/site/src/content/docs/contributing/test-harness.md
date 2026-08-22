---
title: "Test Harness"
---

`otter test` runs `node:test` files. A test file is an ordinary program that
requires or imports `node:test`; there is no fixture format and no metadata
header.

```bash
otter test                 # every test file under ./test
otter test path/to/file.ts # one file
otter test src tools       # every test file under these directories
```

## Discovery

With no paths, discovery walks `test/`. A path naming a file is taken as-is; a
path naming a directory is walked.

A walked file is a test when its extension is `.js`, `.cjs`, `.mjs`, `.ts`,
`.cts`, or `.mts`, and one of:

- the name starts with `test-` or `test_`;
- the name ends with `.test.<ext>`;
- the file sits under a directory named `test`.

An empty result is an error, not an empty run.

## Running

Tests are registered when declared and run in declaration order, one after
another. `before` and `after` bracket the suite they were declared in;
`beforeEach` and `afterEach` bracket every test under it, outermost first on
the way in and innermost first on the way out. A subtest (`t.test`) runs where
it is declared, inside its parent, and a failed subtest fails the parent.

## Output

Each file prints TAP to stdout: one point per test, `# ` comments for a
failure's detail, then the plan and tally.

```
ok 1 - adds
not ok 2 - fails
# AssertionError: ...
ok 3 - unwritten # TODO
1..3
# tests 3
# pass 1
# fail 1
# todo 1
```

Each file runs in a fresh runtime; its exit code is its result. The command
exits `1` if any file failed, `0` otherwise.

`--json` prints newline-delimited records: one `testPlan` listing the
discovered files, then one `testFile` per file with its path and exit code.

## Snapshots

`t.assert.snapshot(value)` compares against `<test file>.snapshot` beside the
test; `t.assert.fileSnapshot(value, path)` compares against a named file.
`otter test --test-update-snapshots` rewrites them instead of failing where
they differ — writing needs file-write permission, and a run that cannot write
what it was told to update fails.
