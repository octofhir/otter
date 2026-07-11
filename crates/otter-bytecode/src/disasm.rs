//! Text disassembler for [`crate::BytecodeModule`].
//!
//! Identical bytecode produces identical text byte-for-byte; the
//! format is consumed by golden tests.
//!
//! # Contents
//! - [`disassemble`] — render a whole module to a `String`.
//!
//! # Invariants
//! - PC is always rendered as 6 zero-padded decimal digits.
//! - Functions are emitted in `id` order; spans table is sorted by
//!   `pc`.
//!
//! # See also
//! - [`crate::dump`] for the machine-readable form.

use std::fmt::Write;

use crate::{BytecodeModule, Function, Operand, SourceKind};

/// Disassemble `module` into the canonical text form.
#[must_use]
pub fn disassemble(module: &BytecodeModule) -> String {
    let mut out = String::new();
    let kind = match module.source_kind {
        SourceKind::JavaScript => "javascript",
        SourceKind::TypeScript => "typescript",
    };
    let _ = writeln!(
        out,
        "; otter bytecode dump v1 — module={} source_kind={}",
        module.module, kind
    );
    for f in &module.functions {
        write_function(&mut out, f);
    }
    out
}

fn write_function(out: &mut String, f: &Function) {
    let _ = writeln!(out);
    let _ = writeln!(out, "function {} @ span={}-{}", f.name, f.span.0, f.span.1);
    let _ = writeln!(out, "  registers:  {}+{}", f.locals, f.scratch);
    let _ = writeln!(out, "  upvalues:   0");
    let _ = writeln!(out, "  feedback:   0");
    let _ = writeln!(out, "  bytecode:");
    for (pc, instr) in f.code.iter().enumerate() {
        let operands = f.code.operands(instr);
        let mut line = format!("    {pc:06}:  {}", instr.op.mnemonic());
        if !operands.is_empty() {
            line.push_str("  ");
            let mut first = true;
            for operand in operands.iter() {
                if !first {
                    line.push(' ');
                }
                first = false;
                match operand {
                    Operand::Register(r) => {
                        let _ = write!(line, "r{r}");
                    }
                    Operand::ConstIndex(k) => {
                        let _ = write!(line, "k[{k}]");
                    }
                    Operand::Imm32(v) => {
                        let _ = write!(line, "i32:{v}");
                    }
                }
            }
        }
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "  source_spans:");
    for s in &f.spans {
        let _ = writeln!(out, "    pc {:06} -> {}-{}", s.pc, s.span.0, s.span.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Function, Instruction, Op, Operand, SpanEntry};

    #[test]
    fn empty_module_renders_banner_only() {
        let module = BytecodeModule {
            module: "test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
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
        let text = disassemble(&module);
        assert!(text.contains("; otter bytecode dump v1"));
        assert!(text.contains("RETURN  r0"));
    }

    #[test]
    fn all_opcodes_have_disassembly_snapshot() {
        let code: Vec<_> = all_ops()
            .iter()
            .enumerate()
            .map(|(pc, op)| Instruction {
                pc: pc as u32,
                op: *op,
                operands: fixture_operands(*op),
            })
            .collect();
        let spans = code
            .iter()
            .map(|instr| SpanEntry {
                pc: instr.pc,
                span: (instr.pc, instr.pc + 1),
            })
            .collect();
        let module = BytecodeModule {
            module: "all-opcodes.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<all-opcodes>".to_string(),
                span: (0, code.len() as u32),
                locals: 8,
                scratch: 8,
                module_url: "all-opcodes.ts".to_string(),
                code: code.into(),
                spans,
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        };

        let actual = mnemonic_snapshot(&disassemble(&module));
        if std::env::var("DUMP_SNAPSHOT").is_ok() {
            std::fs::write("/tmp/snapshot.txt", &actual).unwrap();
        }
        assert_eq!(actual, ALL_OPCODE_MNEMONIC_SNAPSHOT);
    }

    fn all_ops() -> &'static [Op] {
        &[
            Op::Nop,
            Op::LoadUndefined,
            Op::LoadHole,
            Op::Return,
            Op::LoadString,
            Op::LoadNumber,
            Op::LoadInt32,
            Op::LoadBigInt,
            Op::LoadRegExp,
            Op::QueueMicrotask,
            Op::PromiseNew,
            Op::PromiseCall,
            Op::LoadTrue,
            Op::LoadFalse,
            Op::LoadLength,
            Op::GetStringIndex,
            Op::CallMethodValue,
            Op::Add,
            Op::Sub,
            Op::Mul,
            Op::Div,
            Op::Rem,
            Op::Neg,
            Op::Pow,
            Op::BitwiseAnd,
            Op::BitwiseOr,
            Op::BitwiseXor,
            Op::BitwiseNot,
            Op::Shl,
            Op::Shr,
            Op::Ushr,
            Op::ToNumber,
            Op::Equal,
            Op::NotEqual,
            Op::LessThan,
            Op::LessEq,
            Op::GreaterThan,
            Op::GreaterEq,
            Op::LoadNull,
            Op::LogicalNot,
            Op::ToBoolean,
            Op::Jump,
            Op::JumpIfTrue,
            Op::JumpIfFalse,
            Op::JumpIfNullish,
            Op::LoadLocal,
            Op::StoreLocal,
            Op::TdzError,
            Op::MakeFunction,
            Op::MakeClosure,
            Op::LoadUpvalue,
            Op::StoreUpvalue,
            Op::StoreUpvalueChecked,
            Op::FreshUpvalue,
            Op::Call,
            Op::CallWithThis,
            Op::BindFunction,
            Op::LoadThis,
            Op::LoadNewTarget,
            Op::Throw,
            Op::EnterTry,
            Op::LeaveTry,
            Op::EndFinally,
            Op::NewError,
            Op::GeneratorStart,
            Op::GetIterator,
            Op::GetAsyncIterator,
            Op::IteratorNext,
            Op::IteratorClose,
            Op::IteratorCloseStart,
            Op::IteratorCloseEnd,
            Op::ArrayPush,
            Op::CallSpread,
            Op::New,
            Op::NewSpread,
            Op::SuperConstructSpread,
            Op::BindThisValue,
            Op::LoadSuperProperty,
            Op::LoadSuperElement,
            Op::SetSuperProperty,
            Op::SetSuperElement,
            Op::JumpViaFinally,
            Op::PopParkedFinally,
            Op::GlobalBindingExists,
            Op::StoreGlobalChecked,
            Op::MakeClass,
            Op::MathLoad,
            Op::MathCall,
            Op::CollectRest,
            Op::ReturnValue,
            Op::ReturnUndefined,
            Op::NewObject,
            Op::LoadProperty,
            Op::StoreProperty,
            Op::DeleteProperty,
            Op::GetPrototype,
            Op::SetPrototype,
            Op::NewArray,
            Op::LoadElement,
            Op::StoreElement,
            Op::ArrayLength,
            Op::HasProperty,
            Op::Instanceof,
            Op::Eval,
            Op::IsEvalIntrinsic,
            Op::NewFunction,
            Op::LoadGlobalThis,
            Op::LoadGlobalOrThrow,
            Op::CollectArguments,
            Op::LoadGlobalOrUndefined,
            Op::DefineGlobalVar,
            Op::ImportMetaResolve,
            Op::ImportNamespaceDynamic,
            Op::ImportNamespace,
            Op::ImportNamespaceDeferred,
            Op::EvaluateModule,
            Op::MarkModuleEvaluated,
            Op::StarReexport,
            Op::ModuleNamespaceObject,
            Op::LoadImportBinding,
            Op::PromiseFulfilledOf,
            Op::TemporalLoad,
            Op::NewCollection,
            Op::NewWeakRef,
            Op::NewFinalizationRegistry,
            Op::SymbolLoad,
            Op::TypeOf,
            Op::DeleteElement,
            Op::Await,
            Op::SameValue,
            Op::IsArray,
            Op::LooseEqual,
            Op::LooseNotEqual,
            Op::NewBuiltinError,
            Op::LoadBuiltinError,
            Op::BigIntCall,
            Op::ArrayConstruct,
            Op::ArrayFrom,
            Op::ArrayOf,
            Op::ArrayBufferCall,
            Op::DataViewCall,
            Op::Yield,
            Op::SharedArrayBufferCall,
            Op::ToPrimitive,
            Op::ForInKeys,
            Op::CopyDataProperties,
            Op::DefineOwnProperty,
        ]
    }

    fn fixture_operands(op: Op) -> Vec<Operand> {
        use crate::opcode_schema::opcode_schema;

        let shape = opcode_schema(op).operand_shape;
        let mut operands = shape
            .prefix()
            .expect("every opcode has an authoritative operand prefix")
            .iter()
            .enumerate()
            .map(|(index, spec)| fixture_operand(spec.kind, index as u32))
            .collect::<Vec<_>>();
        if let Some((count_index, tail)) = shape.variadic() {
            operands[count_index] = fixture_operand(
                shape.prefix().expect("variadic prefix")[count_index].kind,
                1,
            );
            operands.push(fixture_operand(tail.kind, operands.len() as u32));
        }
        operands
    }

    fn fixture_operand(kind: crate::opcode_schema::OperandKind, value: u32) -> Operand {
        use crate::opcode_schema::OperandKind;

        match kind {
            OperandKind::Register => Operand::Register(value as u16),
            OperandKind::ConstIndex => Operand::ConstIndex(value),
            OperandKind::Imm32 => Operand::Imm32(value as i32),
        }
    }

    fn mnemonic_snapshot(disassembly: &str) -> String {
        let mut snapshot = String::new();
        for line in disassembly.lines() {
            let trimmed = line.trim_start();
            if let Some((pc, rest)) = trimmed.split_once(":  ")
                && pc.chars().all(|ch| ch.is_ascii_digit())
                && let Some(mnemonic) = rest.split_whitespace().next()
            {
                let _ = writeln!(snapshot, "{pc} {mnemonic}");
            }
        }
        snapshot
    }

    const ALL_OPCODE_MNEMONIC_SNAPSHOT: &str = "\
000000 NOP\n\
000001 LOAD_UNDEFINED\n\
000002 LOAD_HOLE\n\
000003 RETURN\n\
000004 LOAD_STRING\n\
000005 LOAD_NUMBER\n\
000006 LOAD_INT32\n\
000007 LOAD_BIGINT\n\
000008 LOAD_REGEXP\n\
000009 QUEUE_MICROTASK\n\
000010 PROMISE_NEW\n\
000011 PROMISE_CALL\n\
000012 LOAD_TRUE\n\
000013 LOAD_FALSE\n\
000014 LOAD_LENGTH\n\
000015 GET_STRING_INDEX\n\
000016 CALL_METHOD_VALUE\n\
000017 ADD\n\
000018 SUB\n\
000019 MUL\n\
000020 DIV\n\
000021 REM\n\
000022 NEG\n\
000023 POW\n\
000024 BIT_AND\n\
000025 BIT_OR\n\
000026 BIT_XOR\n\
000027 BIT_NOT\n\
000028 SHL\n\
000029 SHR\n\
000030 USHR\n\
000031 TO_NUMBER\n\
000032 EQ\n\
000033 NEQ\n\
000034 LT\n\
000035 LE\n\
000036 GT\n\
000037 GE\n\
000038 LOAD_NULL\n\
000039 NOT\n\
000040 TO_BOOLEAN\n\
000041 JUMP\n\
000042 JUMP_IF_TRUE\n\
000043 JUMP_IF_FALSE\n\
000044 JUMP_IF_NULLISH\n\
000045 LOAD_LOCAL\n\
000046 STORE_LOCAL\n\
000047 TDZ_ERROR\n\
000048 MAKE_FUNCTION\n\
000049 MAKE_CLOSURE\n\
000050 LOAD_UPVALUE\n\
000051 STORE_UPVALUE\n\
000052 STORE_UPVALUE_CHECKED\n\
000053 FRESH_UPVALUE\n\
000054 CALL\n\
000055 CALL_WITH_THIS\n\
000056 BIND_FUNCTION\n\
000057 LOAD_THIS\n\
000058 LOAD_NEW_TARGET\n\
000059 THROW\n\
000060 ENTER_TRY\n\
000061 LEAVE_TRY\n\
000062 END_FINALLY\n\
000063 NEW_ERROR\n\
000064 GENERATOR_START\n\
000065 GET_ITERATOR\n\
000066 GET_ASYNC_ITERATOR\n\
000067 ITERATOR_NEXT\n\
000068 ITERATOR_CLOSE\n\
000069 ITERATOR_CLOSE_START\n\
000070 ITERATOR_CLOSE_END\n\
000071 ARRAY_PUSH\n\
000072 CALL_SPREAD\n\
000073 NEW\n\
000074 NEW_SPREAD\n\
000075 SUPER_CONSTRUCT_SPREAD\n\
000076 BIND_THIS_VALUE\n\
000077 LOAD_SUPER_PROPERTY\n\
000078 LOAD_SUPER_ELEMENT\n\
000079 SET_SUPER_PROPERTY\n\
000080 SET_SUPER_ELEMENT\n\
000081 JUMP_VIA_FINALLY\n\
000082 POP_PARKED_FINALLY\n\
000083 GLOBAL_BINDING_EXISTS\n\
000084 STORE_GLOBAL_CHECKED\n\
000085 MAKE_CLASS\n\
000086 MATH_LOAD\n\
000087 MATH_CALL\n\
000088 COLLECT_REST\n\
000089 RETURN_VALUE\n\
000090 RETURN_UNDEFINED\n\
000091 NEW_OBJECT\n\
000092 LOAD_PROPERTY\n\
000093 STORE_PROPERTY\n\
000094 DELETE_PROPERTY\n\
000095 GET_PROTOTYPE\n\
000096 SET_PROTOTYPE\n\
000097 NEW_ARRAY\n\
000098 LOAD_ELEMENT\n\
000099 STORE_ELEMENT\n\
000100 ARRAY_LENGTH\n\
000101 HAS_PROPERTY\n\
000102 INSTANCEOF\n\
000103 EVAL\n\
000104 IS_EVAL_INTRINSIC\n\
000105 NEW_FUNCTION\n\
000106 LOAD_GLOBAL_THIS\n\
000107 LOAD_GLOBAL_OR_THROW\n\
000108 COLLECT_ARGUMENTS\n\
000109 LOAD_GLOBAL_OR_UNDEFINED\n\
000110 DEFINE_GLOBAL_VAR\n\
000111 IMPORT_META_RESOLVE\n\
000112 IMPORT_NAMESPACE_DYNAMIC\n\
000113 IMPORT_NAMESPACE\n\
000114 IMPORT_NAMESPACE_DEFERRED\n\
000115 EVALUATE_MODULE\n\
000116 MARK_MODULE_EVALUATED\n\
000117 STAR_REEXPORT\n\
000118 MODULE_NAMESPACE_OBJECT\n\
000119 LOAD_IMPORT_BINDING\n\
000120 PROMISE_FULFILLED_OF\n\
000121 TEMPORAL_LOAD\n\
000122 NEW_COLLECTION\n\
000123 NEW_WEAK_REF\n\
000124 NEW_FINALIZATION_REGISTRY\n\
000125 SYMBOL_LOAD\n\
000126 TYPEOF\n\
000127 DELETE_ELEMENT\n\
000128 AWAIT\n\
000129 SAME_VALUE\n\
000130 IS_ARRAY\n\
000131 LOOSE_EQ\n\
000132 LOOSE_NEQ\n\
000133 NEW_BUILTIN_ERROR\n\
000134 LOAD_BUILTIN_ERROR\n\
000135 BIGINT_CALL\n\
000136 ARRAY_CONSTRUCT\n\
000137 ARRAY_FROM\n\
000138 ARRAY_OF\n\
000139 ARRAY_BUFFER_CALL\n\
000140 DATA_VIEW_CALL\n\
000141 YIELD\n\
000142 SHARED_ARRAY_BUFFER_CALL\n\
000143 TO_PRIMITIVE\n\
000144 FOR_IN_KEYS\n\
000145 COPY_DATA_PROPERTIES\n\
000146 DEFINE_OWN_PROPERTY\n";
}
