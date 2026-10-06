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
    use crate::{
        EvalCallerChain, EvalCallerScope, Function, Instruction, Op, Operand, ScopeDescriptor,
        ScopeFlags, ScopeKind, SlotDescriptor, SlotKind, SourceKind, SpanEntry,
    };

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
                spans: crate::SpanTable::new(&[SpanEntry {
                    pc: 0,
                    span: (0, 0),
                }]),
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        };
        let json = to_json_pretty(&module).unwrap();
        assert!(json.contains("\"module\": \"x.ts\""));
        assert!(json.contains("\"source_kind\": \"typescript\""));
    }

    #[test]
    fn scope_descriptors_dump_and_parse_back() {
        let scope = ScopeDescriptor {
            kind: ScopeKind::Params,
            flags: ScopeFlags {
                strict: false,
                var_scope: true,
                has_extension: true,
            },
            slots: vec![
                SlotDescriptor {
                    name: "a".to_string(),
                    kind: SlotKind::Param { checked: true },
                    exported: false,
                },
                SlotDescriptor {
                    name: "x".to_string(),
                    kind: SlotKind::Var,
                    exported: true,
                },
            ],
        };
        let json = serde_json::to_value(&scope).unwrap();
        assert_eq!(json["kind"], "params");
        assert_eq!(json["slots"][0]["kind"]["param"]["checked"], true);
        assert_eq!(json["slots"][1]["kind"], "var");
        assert_eq!(
            serde_json::from_value::<ScopeDescriptor>(json).unwrap(),
            scope
        );

        let chain = EvalCallerChain {
            scopes: vec![EvalCallerScope {
                descriptor: scope,
                extension_names: vec!["injected".to_string()],
            }],
            var_depth: Some(0),
        };
        let round_trip: EvalCallerChain =
            serde_json::from_str(&serde_json::to_string(&chain).unwrap()).unwrap();
        assert_eq!(round_trip, chain);
    }
}
