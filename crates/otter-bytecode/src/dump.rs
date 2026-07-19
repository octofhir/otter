//! JSON dump for [`crate::BytecodeModule`].
//!
//! # Contents
//! - [`to_json_pretty`] — serialize a module to pretty JSON suitable
//!   for golden files.

use crate::BytecodeModule;

/// Serialize `module` as pretty JSON (2-space indent, trailing
/// newline). Used for `--dump-bytecode=json` and golden files.
///
/// # Errors
/// Returns [`serde_json::Error`] only on internal serialization
/// failure; the foundation types implement infallible
/// [`serde::Serialize`].
pub fn to_json_pretty(module: &BytecodeModule) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string_pretty(module)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Function, Instruction, Op, Operand, SourceKind, SpanEntry};

    #[test]
    fn dump_carries_bytecode_module() {
        let module = BytecodeModule {
            module: "x.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                scratch: 1,
                code: vec![Instruction {
                    pc: 0,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                }]
                .into(),
                spans: vec![SpanEntry {
                    pc: 0,
                    span: (0, 0),
                }],
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };
        let json = to_json_pretty(&module).unwrap();
        assert!(json.contains("\"module\": \"x.ts\""));
        assert!(json.contains("\"source_kind\": \"typescript\""));
    }
}
