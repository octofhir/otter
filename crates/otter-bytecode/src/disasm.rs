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
    for instr in &f.code {
        let mut line = format!("    {:06}:  {}", instr.pc, instr.op.mnemonic());
        if !instr.operands.is_empty() {
            line.push_str("  ");
            let mut first = true;
            for operand in &instr.operands {
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
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                code: vec![Instruction {
                    pc: 0,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)].into(),
                }],
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
                operands: fixture_operands(*op).into(),
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
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<all-opcodes>".to_string(),
                span: (0, code.len() as u32),
                locals: 8,
                scratch: 8,
                module_url: "all-opcodes.ts".to_string(),
                code,
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
            Op::MakeClass,
            Op::MathLoad,
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
            Op::NewIntl,
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
        match op {
            Op::Nop | Op::ReturnUndefined | Op::LeaveTry | Op::EndFinally | Op::GeneratorStart => {
                Vec::new()
            }
            Op::LoadUndefined
            | Op::LoadHole
            | Op::Return
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadNull
            | Op::LoadThis
            | Op::LoadNewTarget
            | Op::Throw
            | Op::CollectRest
            | Op::ReturnValue
            | Op::NewObject
            | Op::CollectArguments
            | Op::BindThisValue
            | Op::LoadGlobalThis => vec![reg(0)],
            Op::MarkModuleEvaluated => vec![konst(0)],
            Op::DeclareGlobalVar => vec![konst(0), imm(1)],
            Op::LoadDynamic | Op::StoreDynamic | Op::TypeofDynamic => vec![reg(0), konst(1)],
            Op::DefineGlobalFunction => vec![konst(0), reg(1), imm(1)],
            Op::DeclareGlobalLex | Op::ValidateGlobalDecl => vec![konst(0), imm(1)],
            Op::StoreGlobalBinding => vec![reg(0), konst(1), imm(1)],
            Op::InitGlobalLex => vec![reg(0), konst(1)],
            Op::EvaluateModule => vec![reg(0), konst(1)],
            Op::Jump | Op::TdzError | Op::FreshUpvalue => vec![imm(-1)],
            Op::JumpViaFinally => vec![imm(-1), imm(1)],
            Op::LoadString
            | Op::LoadNumber
            | Op::LoadInt32
            | Op::LoadBigInt
            | Op::LoadRegExp
            | Op::LoadLength
            | Op::Neg
            | Op::BitwiseNot
            | Op::ToNumber
            | Op::LogicalNot
            | Op::ToBoolean
            | Op::MakeFunction
            | Op::MathLoad
            | Op::ImportNamespace
            | Op::ImportNamespaceDeferred
            | Op::ModuleNamespaceObject
            | Op::PromiseFulfilledOf
            | Op::NewWeakRef
            | Op::NewFinalizationRegistry
            | Op::SymbolLoad
            | Op::TypeOf
            | Op::Await
            | Op::IsArray
            | Op::LoadBuiltinError
            | Op::BigIntCall
            | Op::ArrayConstruct
            | Op::ArrayFrom
            | Op::ArrayOf
            | Op::ArrayBufferCall
            | Op::DataViewCall
            | Op::SharedArrayBufferCall
            | Op::LoadGlobalOrThrow
            | Op::LoadGlobalOrUndefined
            | Op::ImportMetaResolve
            | Op::ImportNamespaceDynamic
            | Op::ForInKeys
            | Op::CopyDataProperties
            | Op::StarReexport
            | Op::Yield
            | Op::ToObject
            | Op::ToNumeric => vec![reg(0), reg(1)],
            Op::DefineGlobalVar => vec![konst(0), reg(1)],
            Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => vec![imm(2), reg(1)],
            Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadUpvalue
            | Op::StoreUpvalue
            | Op::StoreUpvalueChecked => {
                vec![reg(0), imm(1)]
            }
            Op::GetStringIndex
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Pow
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::Equal
            | Op::NotEqual
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::LoadProperty
            | Op::LoadSuperProperty
            | Op::LoadSuperElement
            | Op::SetSuperProperty
            | Op::SetSuperElement
            | Op::DeleteProperty
            | Op::GetPrototype
            | Op::SetPrototype
            | Op::ArrayLength
            | Op::NewError
            | Op::GetIterator
            | Op::ArrayPush
            | Op::NewSpread
            | Op::SuperConstructSpread
            | Op::NewCollection
            | Op::LoadElement
            | Op::DeleteElement
            | Op::HasProperty
            | Op::Instanceof
            | Op::SameValue
            | Op::LooseEqual
            | Op::LooseNotEqual
            | Op::NewBuiltinError
            | Op::ToPrimitive
            | Op::PromiseCall
            | Op::LoadImportBinding
            | Op::DefineOwnProperty
            | Op::PrivateGet
            | Op::PrivateSet
            | Op::YieldDelegate
            | Op::DefineDataProperty => vec![reg(0), reg(1), reg(2)],
            Op::SetFunctionName => vec![reg(0), reg(1), konst(2)],
            Op::ClassCheck => vec![imm(0), reg(1)],
            Op::ToPropertyKey => vec![reg(0), reg(1)],
            Op::IteratorNext => vec![reg(0), reg(1), reg(2)],
            Op::GetAsyncIterator => vec![reg(0), reg(1)],
            Op::IteratorClose | Op::IteratorCloseStart | Op::IteratorCloseEnd => vec![reg(0)],
            Op::CallSpread | Op::New | Op::MakeClass | Op::StoreProperty | Op::StoreElement => {
                vec![reg(0), reg(1), reg(2), reg(3)]
            }
            Op::CallMethodValue | Op::CallWithThis | Op::BindFunction | Op::NewIntl => {
                vec![reg(0), reg(1), reg(2), reg(3)]
            }
            Op::Call => vec![reg(0), reg(1), reg(2)],
            Op::QueueMicrotask => vec![reg(0), reg(1)],
            Op::PromiseNew => vec![reg(0), reg(1), reg(2)],
            Op::MakeClosure => vec![reg(0), konst(1), imm(1), imm(2)],
            Op::EnterTry => vec![imm(1), imm(2), reg(3)],
            Op::Eval | Op::NewFunction => vec![reg(0), reg(1)],
            Op::NewArray => vec![reg(0), reg(1), reg(2)],
            Op::TemporalLoad => vec![reg(0), reg(1)],
        }
    }

    fn reg(value: u16) -> Operand {
        Operand::Register(value)
    }

    fn konst(value: u32) -> Operand {
        Operand::ConstIndex(value)
    }

    fn imm(value: i32) -> Operand {
        Operand::Imm32(value)
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
000000 NOP
000001 LOAD_UNDEFINED
000002 LOAD_HOLE
000003 RETURN
000004 LOAD_STRING
000005 LOAD_NUMBER
000006 LOAD_INT32
000007 LOAD_BIGINT
000008 LOAD_REGEXP
000009 QUEUE_MICROTASK
000010 PROMISE_NEW
000011 PROMISE_CALL
000012 LOAD_TRUE
000013 LOAD_FALSE
000014 LOAD_LENGTH
000015 GET_STRING_INDEX
000016 CALL_METHOD_VALUE
000017 ADD
000018 SUB
000019 MUL
000020 DIV
000021 REM
000022 NEG
000023 POW
000024 BIT_AND
000025 BIT_OR
000026 BIT_XOR
000027 BIT_NOT
000028 SHL
000029 SHR
000030 USHR
000031 TO_NUMBER
000032 EQ
000033 NEQ
000034 LT
000035 LE
000036 GT
000037 GE
000038 LOAD_NULL
000039 NOT
000040 TO_BOOLEAN
000041 JUMP
000042 JUMP_IF_TRUE
000043 JUMP_IF_FALSE
000044 JUMP_IF_NULLISH
000045 LOAD_LOCAL
000046 STORE_LOCAL
000047 TDZ_ERROR
000048 MAKE_FUNCTION
000049 MAKE_CLOSURE
000050 LOAD_UPVALUE
000051 STORE_UPVALUE
000052 STORE_UPVALUE_CHECKED
000053 FRESH_UPVALUE
000054 CALL
000055 CALL_WITH_THIS
000056 BIND_FUNCTION
000057 LOAD_THIS
000058 LOAD_NEW_TARGET
000059 THROW
000060 ENTER_TRY
000061 LEAVE_TRY
000062 END_FINALLY
000063 NEW_ERROR
000064 GENERATOR_START
000065 GET_ITERATOR
000066 GET_ASYNC_ITERATOR
000067 ITERATOR_NEXT
000068 ITERATOR_CLOSE
000069 ITERATOR_CLOSE_START
000070 ITERATOR_CLOSE_END
000071 ARRAY_PUSH
000072 CALL_SPREAD
000073 NEW
000074 NEW_SPREAD
000075 SUPER_CONSTRUCT_SPREAD
000076 BIND_THIS_VALUE
000077 LOAD_SUPER_PROPERTY
000078 LOAD_SUPER_ELEMENT
000079 SET_SUPER_PROPERTY
000080 SET_SUPER_ELEMENT
000081 JUMP_VIA_FINALLY
000082 MAKE_CLASS
000083 MATH_LOAD
000084 COLLECT_REST
000085 RETURN_VALUE
000086 RETURN_UNDEFINED
000087 NEW_OBJECT
000088 LOAD_PROPERTY
000089 STORE_PROPERTY
000090 DELETE_PROPERTY
000091 GET_PROTOTYPE
000092 SET_PROTOTYPE
000093 NEW_ARRAY
000094 LOAD_ELEMENT
000095 STORE_ELEMENT
000096 ARRAY_LENGTH
000097 HAS_PROPERTY
000098 INSTANCEOF
000099 EVAL
000100 NEW_FUNCTION
000101 LOAD_GLOBAL_THIS
000102 LOAD_GLOBAL_OR_THROW
000103 COLLECT_ARGUMENTS
000104 LOAD_GLOBAL_OR_UNDEFINED
000105 DEFINE_GLOBAL_VAR
000106 IMPORT_META_RESOLVE
000107 IMPORT_NAMESPACE_DYNAMIC
000108 IMPORT_NAMESPACE
000109 IMPORT_NAMESPACE_DEFERRED
000110 EVALUATE_MODULE
000111 MARK_MODULE_EVALUATED
000112 STAR_REEXPORT
000113 MODULE_NAMESPACE_OBJECT
000114 LOAD_IMPORT_BINDING
000115 PROMISE_FULFILLED_OF
000116 NEW_INTL
000117 TEMPORAL_LOAD
000118 NEW_COLLECTION
000119 NEW_WEAK_REF
000120 NEW_FINALIZATION_REGISTRY
000121 SYMBOL_LOAD
000122 TYPEOF
000123 DELETE_ELEMENT
000124 AWAIT
000125 SAME_VALUE
000126 IS_ARRAY
000127 LOOSE_EQ
000128 LOOSE_NEQ
000129 NEW_BUILTIN_ERROR
000130 LOAD_BUILTIN_ERROR
000131 BIGINT_CALL
000132 ARRAY_CONSTRUCT
000133 ARRAY_FROM
000134 ARRAY_OF
000135 ARRAY_BUFFER_CALL
000136 DATA_VIEW_CALL
000137 YIELD
000138 SHARED_ARRAY_BUFFER_CALL
000139 TO_PRIMITIVE
000140 FOR_IN_KEYS
000141 COPY_DATA_PROPERTIES
000142 DEFINE_OWN_PROPERTY
";
}
