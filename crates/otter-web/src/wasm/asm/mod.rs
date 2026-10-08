//! asm.js modules run as WebAssembly (V8 `src/asmjs`).
//!
//! A function whose directive prologue holds `"use asm"` is offered, on its
//! first interpreted entry, to the realm's `__otterAsmLink` global: the
//! linker validates the source as asm.js, translates it to a WebAssembly
//! module against the actual heap, links it, and returns the exports in
//! place of running the body. Anything outside asm.js, or any failed link
//! check, leaves the body to run as ordinary JavaScript, so the result is
//! always what the JavaScript computes.
//!
//! # Contents
//! - `types` — the asm.js type lattice.
//! - `translate` — validation and translation over the oxc AST.
//! - `instance` — linking: stdlib checks, the heap as memory, imports and
//!   exports; [`link_native`] is the global hook.
//!
//! # Invariants
//! - The VM side offers a module once per call, before its body runs
//!   (`otter_vm::interp::call_dispatch`), and treats `undefined` as "run the
//!   body".
//! - A linked module's heap buffer is keyed against detach for its lifetime.
//!
//! # See also
//! - <http://asmjs.org/spec/latest/>
//! - `otter_runtime::asm_stdlib` — standard-library identity and arithmetic.

mod instance;
mod translate;
mod types;

pub(crate) use instance::link_native;

#[cfg(test)]
mod tests;
