# Startup Performance

Otter treats startup as a production requirement. Contributor-friendly APIs
must not silently make `RuntimeBuilder::build()` or first script execution
slower.

Task 98 owns the startup ratchet. The benchmark set should cover:

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
