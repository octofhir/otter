//! Runtime-bridge declarations for the Cranelift backend.
//!
//! When the lowered subset re-enters the VM — `Call` / `CallMethod` /
//! `MakeFunction` — the emitted code calls the *same* `extern "C"` bridge thunks
//! the dynasm tier uses (`jit_runtime_call`, `jit_runtime_make_function`),
//! declared to Cranelift as external functions and resolved to their real
//! addresses through the `JITModule` symbol table (CRANELIFT_TIER2.md §5).
//!
//! The Stage S0 numeric subset has **no** re-entry: it contains no `Call`,
//! `CallMethod`, or `MakeFunction` node (those are rejected by
//! [`super::check_supported`], so such a graph falls back to the dynasm tier).
//! No external functions are therefore declared yet, and no symbols are
//! registered on the `JITBuilder`. This module is the home for that wiring as the
//! call opcodes come online; it intentionally holds no code until then so the S0
//! backend has no unexercised bridge surface.
//!
//! # See also
//! - [`super::lower`] — where call-node lowering will emit the `call` to these.
//! - `crate::baseline` — the bridge thunks to be reused verbatim.
