//! Inline Cache (IC) subsystem.
//!
//! Manages IC sites per function and provides a CacheIR interpreter
//! for executing IC stubs without native compilation.
//!
//! ## Architecture
//!
//! ```text
//! Interpreter → IC dispatch → CacheIR interpreter → fast result
//!                           ↓ (miss)
//!                     Runtime slow path → generate CacheIR → attach stub
//! ```
//!
//! The CacheIR interpreter is the first consumer. Later, the baseline JIT
//! will compile CacheIR to native stubs, and the speculative tier will
//! transpile CacheIR to MIR.

pub mod interpret;
pub mod manager;

pub use interpret::interpret_cache_ir;
pub use manager::ICManager;
