//! JSON dump for [`crate::BytecodeModule`].
//!
//! Output format is locked by
//! [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   ) §2. Top-level wrapper carries `otterBytecodeDumpVersion: 1`.
//!
//! # Contents
//! - [`DUMP_SCHEMA_VERSION`] — pinned at `1`.
//! - [`to_json_pretty`] — serialize a module to pretty JSON suitable
//!   for golden files.
//! - [`Dump`] — wrapper struct that adds the version banner.

use serde::Serialize;

use crate::BytecodeModule;

/// Current dump schema version. Bump per spec §6 only.
pub const DUMP_SCHEMA_VERSION: u32 = 1;

/// Top-level wrapper that prepends the schema-version field.
#[derive(Debug, Serialize)]
pub struct Dump<'a> {
    /// Pinned at [`DUMP_SCHEMA_VERSION`].
    #[serde(rename = "otterBytecodeDumpVersion")]
    pub version: u32,
    /// The module being dumped.
    #[serde(flatten)]
    pub module: &'a BytecodeModule,
}

/// Serialize `module` as pretty JSON (2-space indent, trailing
/// newline). Used for `--dump-bytecode=json` and golden files.
///
/// # Errors
/// Returns [`serde_json::Error`] only on internal serialization
/// failure; the foundation types implement infallible
/// [`serde::Serialize`].
pub fn to_json_pretty(module: &BytecodeModule) -> Result<String, serde_json::Error> {
    let dump = Dump {
        version: DUMP_SCHEMA_VERSION,
        module,
    };
    let mut s = serde_json::to_string_pretty(&dump)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Function, Instruction, Op, Operand, SourceKind, SpanEntry};

    #[test]
    fn dump_carries_schema_version() {
        let module = BytecodeModule {
            module: "x.ts".to_string(),
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 1,
                param_count: 0,
                own_upvalue_count: 0,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                module_url: String::new(),
                code: vec![Instruction {
                    pc: 0,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                }],
                spans: vec![SpanEntry {
                    pc: 0,
                    span: (0, 0),
                }],
            }],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let json = to_json_pretty(&module).unwrap();
        assert!(json.contains("\"otterBytecodeDumpVersion\": 1"));
        assert!(json.contains("\"source_kind\": \"typescript\""));
    }
}
