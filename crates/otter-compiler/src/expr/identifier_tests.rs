//! Identifier binding proofs for immutable global constants.
//!
//! # Contents
//! - Ordinary register/global reads and canonical constant loads.
//! - Lexical, captured, eval, with and module-import interception.
//!
//! # Invariants
//! Every fixture passes bytecode and context-chain verification. Constant
//! folding cannot replace an observable environment binding read.
//!
//! # See also
//! - `identifier` owns identifier-expression lowering.
//! - `context_tests` verifies emitted context coordinates.

use crate::{SyntaxSourceKind, compile_script_source};
use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand};

fn compile(source: &str) -> BytecodeModule {
    let module = compile_script_source(source, SyntaxSourceKind::JavaScript, "identifiers.js")
        .expect("identifier proof fixture");
    crate::context_tests::assert_verified(&module);
    module
}

fn function<'a>(module: &'a BytecodeModule, name: &str) -> &'a Function {
    module
        .functions
        .iter()
        .find(|function| function.name == name)
        .expect("fixture function")
}

fn is_name(module: &BytecodeModule, operand: Option<Operand>, name: &str) -> bool {
    let Some(Operand::ConstIndex(index)) = operand else {
        return false;
    };
    matches!(module.constants.get(index as usize), Some(Constant::String { utf16 })
        if utf16.iter().copied().eq(name.encode_utf16()))
}

fn number_loads(module: &BytecodeModule, name: &str) -> Vec<f64> {
    let code = &function(module, name).code;
    code.iter()
        .filter(|instruction| instruction.op == Op::LoadNumber)
        .map(|instruction| {
            let Some(Operand::ConstIndex(index)) = code.operand(instruction, 1) else {
                panic!("number constant operand");
            };
            let Constant::Number { bits } = module.constants[index as usize] else {
                panic!("number constant");
            };
            f64::from_bits(bits)
        })
        .collect()
}

#[test]
fn ordinary_names_keep_their_register_and_global_owners() {
    let module =
        compile("function local(value){return value;} function global(){return ordinaryGlobal;}");
    // A parameter is returned from its own register, without a copy.
    let local = &function(&module, "local").code;
    assert_eq!(local.iter().filter(|i| i.op == Op::LoadLocal).count(), 0);
    let ret = local
        .iter()
        .find(|i| i.op == Op::ReturnValue)
        .expect("return");
    assert_eq!(local.operand(ret, 0), Some(Operand::Register(0)));
    let global = &function(&module, "global").code;
    let reads: Vec<_> = global
        .iter()
        .filter(|i| i.op == Op::LoadGlobalOrThrow)
        .collect();
    assert_eq!(reads.len(), 1);
    assert!(is_name(
        &module,
        global.operand(reads[0], 1),
        "ordinaryGlobal"
    ));
    assert!(number_loads(&module, "local").is_empty());
    assert!(number_loads(&module, "global").is_empty());
}

#[test]
fn canonical_constants_load_exact_numeric_values() {
    let module = compile("function read(){return [NaN,Infinity];}");
    let values = number_loads(&module, "read");
    assert_eq!(values.len(), 2);
    assert!(values[0].is_nan());
    assert_eq!(values[1], f64::INFINITY);
    assert!(!function(&module, "read").code.iter().any(|i| matches!(
        i.op,
        Op::LoadGlobalOrThrow | Op::LoadLookupGlobal | Op::LoadImportBinding
    )));
}

#[test]
fn parameter_shadows_read_distinct_formal_registers() {
    let module = compile("function read(NaN,Infinity){return [NaN,Infinity];}");
    let code = &function(&module, "read").code;
    let sources: Vec<_> = code
        .iter()
        .filter(|i| i.op == Op::LoadLocal)
        .map(|i| code.operand(i, 1))
        .collect();
    assert_eq!(sources.len(), 2);
    assert_ne!(sources[0], sources[1]);
    assert!(number_loads(&module, "read").is_empty());
}

#[test]
fn captured_shadows_use_verified_context_reads() {
    let module = compile(
        "function outer(){let NaN=3;let Infinity=5;return function read(){return [NaN,Infinity];};}",
    );
    assert_eq!(
        function(&module, "read")
            .code
            .iter()
            .filter(|i| matches!(i.op, Op::LoadContextSlot | Op::LoadContextSlotChecked))
            .count(),
        2
    );
    assert!(number_loads(&module, "read").is_empty());
}

#[test]
fn lexical_reads_preserve_early_and_cross_clause_tdz() {
    let module = compile(
        "function early(){return NaN;let NaN=1;} function cross(flag){switch(flag){case 0: return Infinity;case 1: let Infinity=2;return Infinity;}}",
    );
    assert!(
        function(&module, "early")
            .code
            .iter()
            .any(|i| i.op == Op::TdzError)
    );
    assert!(
        function(&module, "cross")
            .code
            .iter()
            .any(|i| i.op == Op::LoadContextSlotChecked)
    );
    assert!(number_loads(&module, "early").is_empty());
    assert!(number_loads(&module, "cross").is_empty());
}

#[test]
fn direct_eval_reads_retain_extension_lookup() {
    let module =
        compile("function read(){eval('var NaN=11;var Infinity=13');return [NaN,Infinity];}");
    let code = &function(&module, "read").code;
    for name in ["NaN", "Infinity"] {
        assert_eq!(
            code.iter()
                .filter(
                    |i| i.op == Op::LoadLookupGlobal && is_name(&module, code.operand(i, 2), name)
                )
                .count(),
            1
        );
    }
    assert!(number_loads(&module, "read").is_empty());
}

#[test]
fn with_reads_probe_before_the_global_fallback() {
    let module = compile("function read(env){with(env){return [NaN,Infinity];}}");
    let code = &function(&module, "read").code;
    assert!(code.iter().any(|i| i.op == Op::HasProperty));
    for name in ["NaN", "Infinity"] {
        assert!(
            code.iter().any(
                |i| i.op == Op::LoadGlobalOrThrow && is_name(&module, code.operand(i, 1), name)
            )
        );
    }
    assert!(number_loads(&module, "read").is_empty());
}

#[test]
fn imported_constant_names_read_the_resolved_export() {
    let host = crate::ModuleHostInfo {
        module_url: "file:///identifiers/main.js".into(),
        resolved_imports: [(
            crate::ImportRequest::plain("./constants.js"),
            "file:///identifiers/constants.js".into(),
        )]
        .into_iter()
        .collect(),
    };
    let module = otter_syntax::with_program(
        "import {value as NaN,other as Infinity} from './constants.js';export function read(){return [NaN,Infinity];}",
        SyntaxSourceKind::JavaScript,
        |program| crate::compile_module_program(program, SyntaxSourceKind::JavaScript, &host, crate::CompileOptions::default()),
    ).expect("module syntax").expect("module lowering");
    crate::context_tests::assert_verified(&module);
    let code = &function(&module, "read").code;
    for name in ["value", "other"] {
        assert_eq!(
            code.iter()
                .filter(|i| i.op == Op::LoadImportBinding
                    && is_name(
                        &module,
                        code.operand(i, 1),
                        "file:///identifiers/constants.js"
                    )
                    && is_name(&module, code.operand(i, 2), name))
                .count(),
            1
        );
    }
    assert!(number_loads(&module, "read").is_empty());
}
