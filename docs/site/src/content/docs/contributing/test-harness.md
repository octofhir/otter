---
title: "Test Harness"
---

The active `otter test` harness lives in `crates/otter-test` and is
exposed through the CLI. It discovers JavaScript and TypeScript fixtures,
runs each fixture in a fresh runtime, and reports structured outcomes.

Supported suites:

- `engine`: first-party engine fixtures under `tests/engine`;
- `smoke`: short release smoke tests under `tests/smoke`;
- `test262`: curated Test262 fixtures under `tests/test262-curated`.

Each fixture may carry TOML metadata in the source header. The runner uses
that metadata for expected exit codes and other fixture-level expectations.
When `--json` is enabled, output is newline-delimited JSON records followed
by a summary report using `HARNESS_SCHEMA_VERSION`.

Discovery skips helper/package directories rather than treating them as
standalone tests:

- directory names starting with `_`;
- `node_modules`.

Use focused suite/filter runs for local iteration, then run the relevant
workspace tests before merging harness behavior changes.
