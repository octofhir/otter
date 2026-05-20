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

        assert_eq!(
            mnemonic_snapshot(&disassemble(&module)),
            ALL_OPCODE_MNEMONIC_SNAPSHOT
        );
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
            Op::JsonCall,
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
            Op::GetIterator,
            Op::IteratorNext,
            Op::ArrayPush,
            Op::CallSpread,
            Op::New,
            Op::NewSpread,
            Op::SuperConstructSpread,
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
            Op::NewFunction,
            Op::GlobalCall,
            Op::LoadGlobalThis,
            Op::LoadGlobalOrThrow,
            Op::CollectArguments,
            Op::LoadGlobalOrUndefined,
            Op::ImportMetaResolve,
            Op::ImportNamespaceDynamic,
            Op::ImportNamespace,
            Op::PromiseFulfilledOf,
            Op::NewIntl,
            Op::TemporalCall,
            Op::TemporalLoad,
            Op::NewCollection,
            Op::NewWeakRef,
            Op::NewFinalizationRegistry,
            Op::SymbolLoad,
            Op::SymbolCall,
            Op::TypeOf,
            Op::DeleteElement,
            Op::Await,
            Op::SameValue,
            Op::IsArray,
            Op::LooseEqual,
            Op::LooseNotEqual,
            Op::NewBuiltinError,
            Op::LoadBuiltinError,
            Op::DateCall,
            Op::BigIntCall,
            Op::ArrayConstruct,
            Op::ArrayFrom,
            Op::ArrayOf,
            Op::ObjectCall,
            Op::ArrayBufferCall,
            Op::DataViewCall,
            Op::TypedArrayCall,
            Op::Yield,
            Op::SharedArrayBufferCall,
            Op::ProxyCall,
            Op::ReflectCall,
            Op::IteratorCall,
            Op::ToPrimitive,
        ]
    }

    fn fixture_operands(op: Op) -> Vec<Operand> {
        match op {
            Op::Nop | Op::ReturnUndefined | Op::LeaveTry | Op::EndFinally => Vec::new(),
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
            | Op::LoadGlobalThis => vec![reg(0)],
            Op::Jump | Op::TdzError => vec![imm(-1)],
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
            | Op::PromiseFulfilledOf
            | Op::NewWeakRef
            | Op::NewFinalizationRegistry
            | Op::SymbolLoad
            | Op::TypeOf
            | Op::Await
            | Op::IsArray
            | Op::LoadBuiltinError
            | Op::DateCall
            | Op::BigIntCall
            | Op::ArrayConstruct
            | Op::ArrayFrom
            | Op::ArrayOf
            | Op::ObjectCall
            | Op::ArrayBufferCall
            | Op::DataViewCall
            | Op::SharedArrayBufferCall
            | Op::ProxyCall
            | Op::ReflectCall
            | Op::IteratorCall
            | Op::LoadGlobalOrThrow
            | Op::LoadGlobalOrUndefined
            | Op::ImportMetaResolve
            | Op::ImportNamespaceDynamic
            | Op::Yield => vec![reg(0), reg(1)],
            Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => vec![imm(2), reg(1)],
            Op::LoadLocal | Op::StoreLocal | Op::LoadUpvalue | Op::StoreUpvalue => {
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
            | Op::GlobalCall
            | Op::MathCall
            | Op::JsonCall
            | Op::PromiseCall
            | Op::SymbolCall => vec![reg(0), reg(1), reg(2)],
            Op::IteratorNext => vec![reg(0), reg(1), reg(2)],
            Op::CallSpread
            | Op::New
            | Op::MakeClass
            | Op::StoreProperty
            | Op::StoreElement
            | Op::TypedArrayCall => vec![reg(0), reg(1), reg(2), reg(3)],
            Op::CallMethodValue
            | Op::CallWithThis
            | Op::BindFunction
            | Op::NewIntl
            | Op::TemporalCall => vec![reg(0), reg(1), reg(2), reg(3)],
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
000009 JSON_CALL
000010 QUEUE_MICROTASK
000011 PROMISE_NEW
000012 PROMISE_CALL
000013 LOAD_TRUE
000014 LOAD_FALSE
000015 LOAD_LENGTH
000016 GET_STRING_INDEX
000017 CALL_METHOD_VALUE
000018 ADD
000019 SUB
000020 MUL
000021 DIV
000022 REM
000023 NEG
000024 POW
000025 BIT_AND
000026 BIT_OR
000027 BIT_XOR
000028 BIT_NOT
000029 SHL
000030 SHR
000031 USHR
000032 TO_NUMBER
000033 EQ
000034 NEQ
000035 LT
000036 LE
000037 GT
000038 GE
000039 LOAD_NULL
000040 NOT
000041 TO_BOOLEAN
000042 JUMP
000043 JUMP_IF_TRUE
000044 JUMP_IF_FALSE
000045 JUMP_IF_NULLISH
000046 LOAD_LOCAL
000047 STORE_LOCAL
000048 TDZ_ERROR
000049 MAKE_FUNCTION
000050 MAKE_CLOSURE
000051 LOAD_UPVALUE
000052 STORE_UPVALUE
000053 CALL
000054 CALL_WITH_THIS
000055 BIND_FUNCTION
000056 LOAD_THIS
000057 LOAD_NEW_TARGET
000058 THROW
000059 ENTER_TRY
000060 LEAVE_TRY
000061 END_FINALLY
000062 NEW_ERROR
000063 GET_ITERATOR
000064 ITERATOR_NEXT
000065 ARRAY_PUSH
000066 CALL_SPREAD
000067 NEW
000068 NEW_SPREAD
000069 MAKE_CLASS
000070 MATH_LOAD
000071 MATH_CALL
000072 COLLECT_REST
000073 RETURN_VALUE
000074 RETURN_UNDEFINED
000075 NEW_OBJECT
000076 LOAD_PROPERTY
000077 STORE_PROPERTY
000078 DELETE_PROPERTY
000079 GET_PROTOTYPE
000080 SET_PROTOTYPE
000081 NEW_ARRAY
000082 LOAD_ELEMENT
000083 STORE_ELEMENT
000084 ARRAY_LENGTH
000085 HAS_PROPERTY
000086 INSTANCEOF
000087 EVAL
000088 NEW_FUNCTION
000089 GLOBAL_CALL
000090 LOAD_GLOBAL_THIS
000091 LOAD_GLOBAL_OR_THROW
000092 COLLECT_ARGUMENTS
000093 LOAD_GLOBAL_OR_UNDEFINED
000094 IMPORT_META_RESOLVE
000095 IMPORT_NAMESPACE_DYNAMIC
000096 IMPORT_NAMESPACE
000097 PROMISE_FULFILLED_OF
000098 NEW_INTL
000099 TEMPORAL_CALL
000100 TEMPORAL_LOAD
000101 NEW_COLLECTION
000102 NEW_WEAK_REF
000103 NEW_FINALIZATION_REGISTRY
000104 SYMBOL_LOAD
000105 SYMBOL_CALL
000106 TYPEOF
000107 DELETE_ELEMENT
000108 AWAIT
000109 SAME_VALUE
000110 IS_ARRAY
000111 LOOSE_EQ
000112 LOOSE_NEQ
000113 NEW_BUILTIN_ERROR
000114 LOAD_BUILTIN_ERROR
000115 DATE_CALL
000116 BIGINT_CALL
000117 ARRAY_CONSTRUCT
000118 ARRAY_FROM
000119 ARRAY_OF
000120 OBJECT_CALL
000121 ARRAY_BUFFER_CALL
000122 DATA_VIEW_CALL
000123 TYPED_ARRAY_CALL
000124 YIELD
000125 SHARED_ARRAY_BUFFER_CALL
000126 PROXY_CALL
000127 REFLECT_CALL
000128 ITERATOR_CALL
000129 TO_PRIMITIVE
";
}
