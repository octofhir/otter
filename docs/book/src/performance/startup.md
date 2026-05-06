# Startup Performance

Otter treats startup as a production requirement. Contributor-friendly APIs
must not silently make `RuntimeBuilder::build()` or first script execution
slower.

Task 98 owns the startup ratchet. Until Task 98 lands, treat the commands
below as the target workflow rather than a stable benchmark suite.

The benchmark set should cover:

- default `RuntimeBuilder::build()`;
- production-config `RuntimeBuilder::build()`;
- first `run_script("undefined;")`;
- CLI cold start for an empty script;
- tiny JavaScript file startup;
- tiny TypeScript file startup when TS lowering is in-process.

Bootstrap telemetry should be opt-in and default-off. Useful counters are:

- objects/functions/prototypes installed;
- strings interned;
- GC allocations and bytes;
- per-bootstrap-phase timing;
- duplicate-name and install-order validation cost.

When changing bootstrap, builders, macro generation, or builtin install
order, include a before/after startup table in the task closeout or PR.

## Local Workflow

Use the repository's benchmark or script target once Task 98 adds it. The
report should include:

```text
case                              before        after         delta
RuntimeBuilder::build(default)    ...           ...           ...
RuntimeBuilder::build(prod)       ...           ...           ...
first run_script("undefined;")    ...           ...           ...
CLI cold empty JS                 ...           ...           ...
CLI cold tiny TS                  ...           ...           ...
```

Always note the build profile, machine, command, sample count, and whether
the run used bootstrap telemetry.

## Regression Policy

High-level contributor APIs must compile down to the same runtime shape as
handwritten static specs. Startup regressions need an explicit production
justification, a benchmark table, and a follow-up plan when accepted.

Lazy or tiered initialization must preserve spec-observable behavior. If a
surface's property enumeration, identity, or initialization timing would
change, do not lazy-install that surface without a spec note and tests.
