# Annex B Eval: Next Fixes

## Current status

- Baseline cluster fixed:
  - `annexB/built-ins/unescape/*` passes.
- Partial Annex B eval progress:
  - `--filter "annexB/language/eval-code" --max-tests 40`
  - Current: `32/40` pass, `8/40` fail.

## Remaining failing tests (current slice)

- `test/annexB/language/eval-code/direct/global-if-decl-else-decl-b-eval-global-existing-non-enumerable-global-init.js`
- `test/annexB/language/eval-code/direct/func-if-stmt-else-decl-eval-func-no-skip-param.js`
- `test/annexB/language/eval-code/direct/func-if-decl-no-else-eval-func-skip-early-err-switch.js`
- `test/annexB/language/eval-code/direct/func-switch-dflt-eval-func-no-skip-param.js`
- `test/annexB/language/eval-code/direct/func-if-decl-else-decl-b-eval-func-init.js`
- `test/annexB/language/eval-code/direct/func-if-decl-else-decl-b-eval-func-no-skip-param.js`
- `test/annexB/language/eval-code/direct/func-if-decl-no-else-eval-func-skip-early-err-try.js`
- `test/annexB/language/eval-code/direct/global-if-stmt-else-decl-eval-global-existing-non-enumerable-global-init.js`

## Root cause hypothesis

The remaining failures are mostly **direct eval environment semantics**, not hoist-only:

- Current direct eval path still relies on global injection (`inject_eval_bindings`), which does not model function env + var env behavior required by Annex B B.3.3.3 in all cases.
- Parameter-collision cases (`...no-skip-param...`) and some `existing-non-enumerable-global` cases need precise env-record semantics.

## Next implementation steps

1. Rework direct eval execution environment in `otter-vm-core`:
   - Replace/limit global-injection approach for direct eval.
   - Execute eval code against a proper function/eval environment mapping (locals/params/var/lexical) instead of temporary global mirroring.
2. Add explicit handling for parameter-name collisions in eval Annex B extension:
   - Ensure `init = f` sees parameter value before branch execution.
   - Ensure post-branch update behavior matches B.3.3.3.
3. Implement correct behavior for existing non-enumerable global properties during extension:
   - Respect descriptor semantics when creating/updating function bindings.
4. Keep skip-early-error checks aligned for nested `switch`/`try` forms in eval code paths.
5. Add focused regression tests in compiler/runtime crate tests for:
   - function-param collision
   - non-enumerable existing global
   - skip-early-error in `switch` and `try` forms

## Suggested debug commands

```bash
cargo run -p otter-test262 --bin test262 -- --filter "annexB/language/eval-code" --max-tests 40
cargo run -p otter-test262 --bin test262 -- tests/test262/test/annexB/language/eval-code/direct/func-if-stmt-else-decl-eval-func-no-skip-param.js -vv
cargo run -p otter-test262 --bin test262 -- tests/test262/test/annexB/language/eval-code/direct/func-if-decl-else-decl-b-eval-func-init.js -vv
```
