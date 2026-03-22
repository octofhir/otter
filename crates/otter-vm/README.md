# otter-vm

Fresh VM backend for Otter.

## Rules

- keep modules small
- prefer splitting files early
- treat files approaching 1000 lines as a design problem
- keep the crate warning-free
- do not port old VM architecture into this crate

## Initial Module Map

- `src/abi.rs`: shared execution ABI versioning and core ABI types
- `src/bridge.rs`: outer engine/runtime integration boundary
- `src/bytecode.rs`: runtime bytecode model
- `src/deopt.rs`: deoptimization metadata
- `src/feedback.rs`: runtime feedback side tables
- `src/frame.rs`: frame and register-window layout
- `src/interpreter.rs`: interpreter entry points
- `src/jit_abi.rs`: JIT-facing ABI boundary
- `src/module.rs`: executable module container

## Growth Policy

When a module starts combining unrelated responsibilities, split it.

Do not wait for a file to become large before decomposing it.
