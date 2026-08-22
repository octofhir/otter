---
title: "Test Harness"
---

`otter test` is Node's test runner. A test file is an ordinary program that
requires or imports `node:test`; there is no fixture format and no metadata
header.

```bash
otter test                 # every test file the runner discovers
otter test path/to/file.ts # one file
otter test 'src/**/*.test.ts' # every file matching a pattern
```

`otter --test <files…>` is the same run: the switch turns any invocation into a
test run, which is how a runner that starts a child process for one file starts
it.

## Discovery

With no paths, the runner walks the working directory for the names Node
recognizes: a file under a `test` directory, a name beginning `test-` or
`test_`, a name of exactly `test`, and any name ending `.test`, `-test`, or
`_test` before a JavaScript or TypeScript extension.

A path naming a file is taken as-is; a path naming a directory is walked; a
glob pattern is expanded.

## Running

Each file runs in its own process, which is what keeps one file's globals out
of another's. `--test-isolation=none` runs them all in this one instead, in the
order they were named.

Tests run in declaration order. `before` and `after` bracket the suite they
were declared in; `beforeEach` and `afterEach` bracket every test under it,
outermost first on the way in and innermost first on the way out. A subtest
(`t.test`) runs inside its parent, and a failed subtest fails the parent.

`--test-concurrency=<n>` sets how many files run at once, `--test-timeout=<ms>`
how long a test may take, `--test-only` and `--test-name-pattern=<re>` which
tests run at all, and `--test-global-setup=<module>` names a module whose
`globalSetup` and `globalTeardown` bracket the whole run.

## Output

`--test-reporter=<name>` chooses what the run prints: `spec` (the default when
output is a terminal), `tap` (the default otherwise), `dot`, `junit`, `lcov`,
or the module a specifier names. `--test-reporter-destination=<path>` sends one
reporter's output to a file; naming both switches more than once runs several
reporters at once.

The command exits `1` if any test failed, `0` otherwise.

## Snapshots

`t.assert.snapshot(value)` compares against `<test file>.snapshot` beside the
test; `t.assert.fileSnapshot(value, path)` compares against a named file.
`otter test --test-update-snapshots` rewrites them instead of failing where
they differ — writing needs file-write permission, and a run that cannot write
what it was told to update fails.
