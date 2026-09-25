# Back-edge relink of baseline loops

Investigation and closing validation: September 25, 2026.

## Starting point

A baseline (Template) body links a call site directly only when its target
already owns entry code at compile time and the site has call feedback. A loop
entered through OSR is compiled after a few iterations, so:

- a target that was still cold (`noEntryGeneration`) stayed behind the generic
  call boundary; entry refresh rebuilds only at function entry, which a long
  loop never reaches;
- a site that had not executed yet (a later branch or phase) had no feedback
  and stayed generic even after its target was compiled;
- a small target never repays an entry compile on its own, so nothing ever
  gave it entry code.

In `jit_stack_owned_runtime_families`, the Template run crossed the generic
boundary 1,974 times for a 1,000-iteration loop.

## Change

- `note_generic_call_target`: when compiled code reaches a bytecode target
  through the generic call/construct boundary and the target owns entry code or
  would repay a direct-call-target compile, the target joins the caller's
  pending set and the next compiled back-edge polls.
- Installing entry code for any pending target also shortens the poll window
  to one back-edge. `request_next_backedge_poll` charges the back-edges already
  consumed, so work-budget accounting stays exact.
- `take_backedge_relink` (baseline frames only): compiles the pending targets
  with the direct-call-target policy, unlinks the body while its activation
  still pins the code, and returns `Relink`. The poll stub maps it to a side
  exit; Template code on AArch64 and x86-64 stamps the loop header PC and exits
  `Interrupt`/`Resume`. The interpreter resumes at the header, which is not
  disabled because the body is gone, and the next OSR links the sites.
- Each `(caller, target)` pair relinks at most once. This bounds rebuilds
  without blocking a site that turns hot after an earlier relink.
- An OSR compile counts the triggering loop's observed trip count as execution
  evidence for direct targets inside that loop. This is the same
  observed-equals-remaining assumption the cost model already applies to the
  loop.
- `receiver_alloc_deopts`, previously never incremented, now counts exact
  `AllocationMiss` exits at construct sites. Newly linked Template construct
  edges made such exits reachable.

## Final paired result

Three alternating pairs, one process per run. The before binary is the
previous gate build, whose relink was limited to one per function; the after
binary is this slice's closing gate build.

- `relink-early`: the callee is warm before the loop, but the loop's site is
  first reached after an earlier site has already relinked the body.
- `relink-late`: the callee is first reached after OSR. Both builds relink
  this case.

| Script / tier | Before ms (pairs) | After ms | Wall | Instructions | Cycles |
| --- | --- | --- | ---: | ---: | ---: |
| relink-early / Production | 1009, 1014, 1013 | 249, 244, 242 | **−75.79%** | −72.77% | −67.83% |
| relink-early / Template (`--jitless`) | 1043, 1037, 1047 | 259, 259, 258 | **−75.18%** | −72.53% | −67.69% |
| relink-late / Production | 242, 241, 242 | 241, 240, 240 | −0.55% | +0.01% | |
| relink-late / Template | 262, 261, 263 | 255, 258, 257 | −2.04% | +0.01% | |

With one relink per function, `relink-early` crossed the generic boundary
about 6 million times; both scripts now run the loop through generated
linkage. The kernel harness calls `engineKernel` repeatedly, and entry refresh
already relinks at those entries, so the cost ledger does not see this effect.

At **289 hand-written production Rust lines added** (24 removed), the
Production gain on `relink-early` is **0.26 percentage points per added line**.

## Validation

- Full gate; differential 42/42.
- 46 runtime binaries pass on AArch64 at `OTTER_GC_STRESS` unset, 16, 4 and 1.
  This includes the new regression
  `a_site_that_turns_hot_after_an_earlier_relink_still_relinks`, which fails
  on the one-relink-per-function build (about 198,000 generic calls against a
  bound of 10,000).
- Under Rosetta, the same set passes except three
  `jit_stack_owned_runtime_families` cases. There, x86-64 Template cannot
  compile the fixture callees (missing opcode coverage), so no direct plan can
  exist. That x86 Template parity work is tracked separately.

[Environment](2026-09-25-backedge-relink-environment.json),
[runs](2026-09-25-backedge-relink-runs.csv).
