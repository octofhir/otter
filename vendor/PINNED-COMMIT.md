# Test262 pinned commit

`vendor/test262` is a `git submodule` pinned to a deliberate commit on
[tc39/test262](https://github.com/tc39/test262)'s `main` branch. The pin
advances only via a dedicated commit that records the upstream changelog
excerpt and a fresh conformance baseline (per
[`docs/new-engine/tasks/100-test262-conformance.md`](../docs/new-engine/tasks/100-test262-conformance.md)).

## Current pin

- **SHA:** `d0c1b4555b03dd404873fd6422a4b5da00136500`
- **Date:** 2026-05-01
- **Rationale:** initial submodule landing for the
  [`crates-next/otter-test262`](../crates-next/otter-test262/) runner.

## Recovering the submodule

If `vendor/test262/` is missing or empty (fresh clone, etc.):

```sh
git submodule update --init --recursive vendor/test262
```

The `crates-next/otter-test262` runner refuses to launch without a
populated `vendor/test262/test/` tree.
