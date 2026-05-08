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
                span: (0, 0),
                locals: 0,
                scratch: 0,
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
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<all-opcodes>".to_string(),
                span: (0, code.len() as u32),
                locals: 8,
                scratch: 8,
                param_count: 0,
                own_upvalue_count: 0,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                module_url: "all-opcodes.ts".to_string(),
                code,
                spans,
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
            Op::StringCall,
            Op::DateCall,
            Op::BigIntCall,
            Op::ArrayCall,
            Op::ObjectCall,
            Op::ArrayBufferCall,
            Op::DataViewCall,
            Op::TypedArrayCall,
            Op::Yield,
            Op::SharedArrayBufferCall,
            Op::AtomicsCall,
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
            | Op::Return
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadNull
            | Op::LoadThis
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
            | Op::StringCall
            | Op::DateCall
            | Op::BigIntCall
            | Op::ArrayCall
            | Op::ObjectCall
            | Op::ArrayBufferCall
            | Op::DataViewCall
            | Op::SharedArrayBufferCall
            | Op::AtomicsCall
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
            | Op::NewCollection
            | Op::LoadElement
            | Op::StoreElement
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
            Op::CallSpread | Op::New | Op::MakeClass | Op::StoreProperty | Op::TypedArrayCall => {
                vec![reg(0), reg(1), reg(2), reg(3)]
            }
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
000002 RETURN
000003 LOAD_STRING
000004 LOAD_NUMBER
000005 LOAD_INT32
000006 LOAD_BIGINT
000007 LOAD_REGEXP
000008 JSON_CALL
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
000052 CALL
000053 CALL_WITH_THIS
000054 BIND_FUNCTION
000055 LOAD_THIS
000056 THROW
000057 ENTER_TRY
000058 LEAVE_TRY
000059 END_FINALLY
000060 NEW_ERROR
000061 GET_ITERATOR
000062 ITERATOR_NEXT
000063 ARRAY_PUSH
000064 CALL_SPREAD
000065 NEW
000066 NEW_SPREAD
000067 MAKE_CLASS
000068 MATH_LOAD
000069 MATH_CALL
000070 COLLECT_REST
000071 RETURN_VALUE
000072 RETURN_UNDEFINED
000073 NEW_OBJECT
000074 LOAD_PROPERTY
000075 STORE_PROPERTY
000076 DELETE_PROPERTY
000077 GET_PROTOTYPE
000078 SET_PROTOTYPE
000079 NEW_ARRAY
000080 LOAD_ELEMENT
000081 STORE_ELEMENT
000082 ARRAY_LENGTH
000083 HAS_PROPERTY
000084 INSTANCEOF
000085 EVAL
000086 NEW_FUNCTION
000087 GLOBAL_CALL
000088 LOAD_GLOBAL_THIS
000089 LOAD_GLOBAL_OR_THROW
000090 COLLECT_ARGUMENTS
000091 LOAD_GLOBAL_OR_UNDEFINED
000092 IMPORT_META_RESOLVE
000093 IMPORT_NAMESPACE_DYNAMIC
000094 IMPORT_NAMESPACE
000095 PROMISE_FULFILLED_OF
000096 NEW_INTL
000097 TEMPORAL_CALL
000098 TEMPORAL_LOAD
000099 NEW_COLLECTION
000100 NEW_WEAK_REF
000101 NEW_FINALIZATION_REGISTRY
000102 SYMBOL_LOAD
000103 SYMBOL_CALL
000104 TYPEOF
000105 DELETE_ELEMENT
000106 AWAIT
000107 SAME_VALUE
000108 IS_ARRAY
000109 LOOSE_EQ
000110 LOOSE_NEQ
000111 NEW_BUILTIN_ERROR
000112 LOAD_BUILTIN_ERROR
000113 STRING_CALL
000114 DATE_CALL
000115 BIGINT_CALL
000116 ARRAY_CALL
000117 OBJECT_CALL
000118 ARRAY_BUFFER_CALL
000119 DATA_VIEW_CALL
000120 TYPED_ARRAY_CALL
000121 YIELD
000122 SHARED_ARRAY_BUFFER_CALL
000123 ATOMICS_CALL
000124 PROXY_CALL
000125 REFLECT_CALL
000126 ITERATOR_CALL
000127 TO_PRIMITIVE
";
}
