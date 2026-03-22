//! Mid-level Intermediate Representation (MIR) for the Otter JIT.
//!
//! The MIR sits between bytecode and Cranelift IR. It is an SSA-form IR
//! with explicit types, guards, and deopt metadata.
//!
//! Pipeline: `bytecode -> MIR -> [optimize] -> CLIF -> machine code`

pub mod types;
pub mod nodes;
pub mod graph;
pub mod display;
pub mod verify;
pub mod builder;
pub mod passes;
