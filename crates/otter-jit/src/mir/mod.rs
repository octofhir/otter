//! Mid-level Intermediate Representation (MIR) for the Otter JIT.
//!
//! The MIR sits between bytecode and Cranelift IR. It is an SSA-form IR
//! with explicit types, guards, and deopt metadata.
//!
//! Pipeline: `bytecode -> MIR -> [optimize] -> CLIF -> machine code`

pub mod builder;
pub mod display;
pub mod graph;
pub mod next_builder;
pub mod nodes;
pub mod passes;
pub mod types;
pub mod verify;
