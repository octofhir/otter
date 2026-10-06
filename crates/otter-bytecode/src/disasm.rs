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
//! - Every operand renders as one whitespace-free token. Packed binding
//!   immediates render through their schema domain — a context coordinate as
//!   `d<depth>:s<slot>`, a scope index as `scope:<n>` — and fall back to
//!   `i32:<value>` when the word does not decode, so unverified input still
//!   disassembles.
//!
//! # See also
//! - [`crate::dump`] for the machine-readable form.
//! - [`crate::opcode_schema::ImmediateDomain`] for the immediate alphabets.

use std::fmt::Write;

use crate::opcode_schema::{ImmediateDomain, operand_spec_at};
use crate::{
    BindingStoreFallback, BytecodeModule, ContextCoord, Function, LookupGlobalMode,
    LookupRefTarget, Op, Operand, ScopeDescriptor, ScopeKind, SlotKind, SourceKind, StoreRefMode,
};

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
        "; otter bytecode dump — module={} source_kind={}",
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
    let _ = writeln!(out, "  scopes:     {}", f.scopes.len());
    for (index, scope) in f.scopes.iter().enumerate() {
        write_scope(out, index, scope);
    }
    let _ = writeln!(out, "  feedback:   0");
    let _ = writeln!(out, "  bytecode:");
    for (pc, instr) in f.code.iter().enumerate() {
        let operands = f.code.operands(instr);
        let mut line = format!("    {pc:06}:  {}", instr.op.mnemonic());
        if !operands.is_empty() {
            line.push_str("  ");
            for (index, operand) in operands.iter().enumerate() {
                if index != 0 {
                    line.push(' ');
                }
                write_operand(&mut line, instr.op, index, operand);
            }
        }
        let _ = writeln!(out, "{line}");
    }
    if !f.handlers.is_empty() {
        let _ = writeln!(out, "  handlers:");
        for handler in &f.handlers {
            let _ = writeln!(
                out,
                "    {:06}..{:06} -> {:06} r{}",
                handler.start, handler.end, handler.target, handler.exception
            );
        }
    }
    let _ = writeln!(out, "  source_spans:");
    for s in &f.spans {
        let _ = writeln!(out, "    pc {:06} -> {}-{}", s.pc, s.span.0, s.span.1);
    }
}

fn write_scope(out: &mut String, index: usize, scope: &ScopeDescriptor) {
    let mut line = format!("    scope {index}: {}", scope_kind_name(scope.kind));
    for (set, flag) in [
        (scope.flags.strict, "strict"),
        (scope.flags.var_scope, "var_scope"),
        (scope.flags.has_extension, "extension"),
    ] {
        if set {
            line.push(' ');
            line.push_str(flag);
        }
    }
    let _ = writeln!(out, "{line}");
    for (slot_index, slot) in scope.slots.iter().enumerate() {
        let exported = if slot.exported { " exported" } else { "" };
        let _ = writeln!(
            out,
            "      slot {slot_index}: {} {}{exported}",
            slot.name,
            slot_kind_name(slot.kind)
        );
    }
}

fn write_operand(line: &mut String, op: Op, index: usize, operand: Operand) {
    match operand {
        Operand::Register(r) => {
            let _ = write!(line, "r{r}");
        }
        Operand::ConstIndex(k) => {
            let _ = write!(line, "k[{k}]");
        }
        Operand::Imm32(value) => {
            let domain = operand_spec_at(op, index).and_then(|spec| spec.imm_domain);
            if !domain.is_some_and(|domain| write_immediate(line, domain, value)) {
                let _ = write!(line, "i32:{value}");
            }
        }
    }
}

/// Render one packed immediate; `false` when the word does not decode.
fn write_immediate(line: &mut String, domain: ImmediateDomain, value: i32) -> bool {
    if !domain.admits(value) {
        return false;
    }
    let _ = match domain {
        ImmediateDomain::ScopeIndex => write!(line, "scope:{value}"),
        ImmediateDomain::ContextDepth => write!(line, "d{value}"),
        ImmediateDomain::ContextCoord => match ContextCoord::from_imm32(value) {
            Some(coord) => write!(line, "d{}:s{}", coord.depth, coord.slot),
            None => return false,
        },
        ImmediateDomain::LookupRefTarget => match LookupRefTarget::from_imm32(value) {
            LookupRefTarget::Slot(coord) => write!(line, "d{}:s{}", coord.depth, coord.slot),
            LookupRefTarget::Global { depth } => write!(line, "d{depth}:global"),
        },
        ImmediateDomain::LookupGlobalMode => match LookupGlobalMode::from_imm32(value) {
            Some(mode) => write!(line, "d{}:{}", mode.depth, strictness(mode.strict)),
            None => return false,
        },
        ImmediateDomain::StoreFallback => match BindingStoreFallback::from_imm32(value) {
            Some(fallback) => write!(line, "{}", fallback_name(fallback)),
            None => return false,
        },
        ImmediateDomain::StoreRefMode => match StoreRefMode::from_imm32(value) {
            Some(mode) => {
                match mode.slot {
                    Some(slot) => {
                        let _ = write!(line, "s{slot}:");
                    }
                    None => line.push_str("global:"),
                }
                write!(
                    line,
                    "{}:{}",
                    fallback_name(mode.fallback),
                    strictness(mode.strict)
                )
            }
            None => return false,
        },
    };
    true
}

const fn strictness(strict: bool) -> &'static str {
    if strict { "strict" } else { "sloppy" }
}

const fn fallback_name(fallback: BindingStoreFallback) -> &'static str {
    match fallback {
        BindingStoreFallback::Mutable => "mutable",
        BindingStoreFallback::ImmutableThrow => "immutable-throw",
        BindingStoreFallback::ImmutableIgnore => "immutable-ignore",
    }
}

const fn scope_kind_name(kind: ScopeKind) -> &'static str {
    match kind {
        ScopeKind::FunctionName => "function_name",
        ScopeKind::Callee => "callee",
        ScopeKind::Params => "params",
        ScopeKind::Body => "body",
        ScopeKind::Lexical => "lexical",
        ScopeKind::Block => "block",
        ScopeKind::Catch => "catch",
        ScopeKind::ForHead => "for_head",
        ScopeKind::Switch => "switch",
        ScopeKind::With => "with",
        ScopeKind::Class => "class",
        ScopeKind::ObjectHome => "object_home",
        ScopeKind::EvalVar => "eval_var",
        ScopeKind::EvalLexical => "eval_lexical",
        ScopeKind::Module => "module",
    }
}

const fn slot_kind_name(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Var => "var",
        SlotKind::FunctionDecl => "function_decl",
        SlotKind::Arguments => "arguments",
        SlotKind::Param { checked: false } => "param",
        SlotKind::Param { checked: true } => "param(checked)",
        SlotKind::Let => "let",
        SlotKind::Const => "const",
        SlotKind::Class => "class",
        SlotKind::DerivedThis => "derived_this",
        SlotKind::FnSelfName => "fn_self_name",
        SlotKind::CatchParam { simple: false } => "catch_param",
        SlotKind::CatchParam { simple: true } => "catch_param(simple)",
        SlotKind::WithObject => "with_object",
        SlotKind::PrivateName => "private_name",
        SlotKind::PrivateBrand => "private_brand",
        SlotKind::SuperHome => "super_home",
        SlotKind::SuperStaticHome => "super_static_home",
        SlotKind::SuperCtor => "super_ctor",
        SlotKind::ClassSelf => "class_self",
        SlotKind::ModuleEnv => "module_env",
        SlotKind::ImportMeta => "import_meta",
        SlotKind::Synthetic => "synthetic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Instruction, ScopeFlags, SlotDescriptor, SpanEntry};

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
        let text = disassemble(&module);
        assert!(text.contains("; otter bytecode dump —"));
        assert!(text.contains("RETURN  r0"));
    }

    #[test]
    fn scopes_and_binding_immediates_render_through_their_domains() {
        let instruction =
            |pc: u32, op: Op, operands: Vec<Operand>| Instruction { pc, op, operands };
        let coord = |depth, slot| ContextCoord::new(depth, slot).unwrap().to_imm32();
        let code = vec![
            instruction(
                0,
                Op::CreateContext,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Imm32(0),
                ],
            ),
            instruction(
                1,
                Op::LoadContextSlot,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Imm32(coord(1, 2)),
                ],
            ),
            instruction(
                2,
                Op::ResolveLookupRef,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Imm32(LookupRefTarget::Global { depth: 3 }.to_imm32()),
                ],
            ),
            instruction(
                3,
                Op::StoreRef,
                vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                    Operand::Imm32(
                        StoreRefMode {
                            slot: Some(4),
                            fallback: BindingStoreFallback::ImmutableThrow,
                            strict: true,
                        }
                        .to_imm32(),
                    ),
                ],
            ),
            instruction(
                4,
                Op::StoreLookupGlobal,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Imm32(
                        LookupGlobalMode {
                            depth: 2,
                            strict: false,
                        }
                        .to_imm32(),
                    ),
                ],
            ),
            instruction(
                5,
                Op::StoreLookupSlot,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Imm32(coord(0, 1)),
                    Operand::Imm32(BindingStoreFallback::ImmutableIgnore.to_imm32()),
                ],
            ),
            instruction(
                6,
                Op::LoadContextSlot,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Imm32(i32::from(u16::MAX)),
                ],
            ),
            instruction(7, Op::ReturnUndefined, vec![]),
        ];
        let module = BytecodeModule {
            module: "scopes.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "f".to_string(),
                scopes: vec![ScopeDescriptor {
                    kind: ScopeKind::Body,
                    flags: ScopeFlags {
                        strict: false,
                        var_scope: true,
                        has_extension: true,
                    },
                    slots: vec![
                        SlotDescriptor {
                            name: "x".to_string(),
                            kind: SlotKind::Var,
                            exported: false,
                        },
                        SlotDescriptor {
                            name: "y".to_string(),
                            kind: SlotKind::Param { checked: true },
                            exported: true,
                        },
                    ],
                }],
                code: code.into(),
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        };
        let text = disassemble(&module);
        for expected in [
            "  scopes:     1\n    scope 0: body var_scope extension\n",
            "      slot 0: x var\n      slot 1: y param(checked) exported\n",
            "CREATE_CONTEXT  r0 r1 scope:0",
            "LOAD_CONTEXT_SLOT  r2 r0 d1:s2",
            "RESOLVE_LOOKUP_REF  r3 r0 k[0] d3:global",
            "STORE_REF  r2 r3 k[0] s4:immutable-throw:strict",
            "STORE_LOOKUP_GLOBAL  r2 r0 k[0] d2:sloppy",
            "STORE_LOOKUP_SLOT  r2 r0 k[0] d0:s1 immutable-ignore",
            "LOAD_CONTEXT_SLOT  r2 r0 i32:65535",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
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
            function_source: None,
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
            Op::LoadClosureContext,
            Op::LoadSelf,
            Op::CreateContext,
            Op::CopyContext,
            Op::LoadContextSlot,
            Op::LoadContextSlotChecked,
            Op::StoreContextSlot,
            Op::StoreContextSlotChecked,
            Op::BindThisContextSlot,
            Op::Call,
            Op::CallWithThis,
            Op::CallForwardArguments,
            Op::BindFunction,
            Op::LoadThis,
            Op::LoadNewTarget,
            Op::Throw,
            Op::NewError,
            Op::GeneratorStart,
            Op::GetIterator,
            Op::GetAsyncIterator,
            Op::IteratorNext,
            Op::IteratorClose,
            Op::IteratorCloseThrow,
            Op::ArrayPush,
            Op::SpreadAppend,
            Op::CallSpread,
            Op::New,
            Op::NewSpread,
            Op::SuperConstructSpread,
            Op::BindThisValue,
            Op::LoadSuperProperty,
            Op::LoadSuperElement,
            Op::SetSuperProperty,
            Op::SetSuperElement,
            Op::GlobalBindingExists,
            Op::StoreGlobalChecked,
            Op::MakeClass,
            Op::MathLoad,
            Op::CollectRest,
            Op::ReturnValue,
            Op::ReturnUndefined,
            Op::ReturnDerived,
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
            Op::TestTypeOf,
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
            Op::NewObjectLiteral,
            Op::LoadLookupSlot,
            Op::StoreLookupSlot,
            Op::DeleteLookupSlot,
            Op::LoadLookupGlobal,
            Op::TypeofLookupGlobal,
            Op::StoreLookupGlobal,
            Op::DeleteLookupGlobal,
            Op::ResolveLookupRef,
            Op::StoreRef,
            Op::DeclareEvalVar,
            Op::StoreVarScope,
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
000050 LOAD_CLOSURE_CONTEXT\n\
000051 LOAD_SELF\n\
000052 CREATE_CONTEXT\n\
000053 COPY_CONTEXT\n\
000054 LOAD_CONTEXT_SLOT\n\
000055 LOAD_CONTEXT_SLOT_CHECKED\n\
000056 STORE_CONTEXT_SLOT\n\
000057 STORE_CONTEXT_SLOT_CHECKED\n\
000058 BIND_THIS_CONTEXT_SLOT\n\
000059 CALL\n\
000060 CALL_WITH_THIS\n\
000061 CALL_FORWARD_ARGUMENTS\n\
000062 BIND_FUNCTION\n\
000063 LOAD_THIS\n\
000064 LOAD_NEW_TARGET\n\
000065 THROW\n\
000066 NEW_ERROR\n\
000067 GENERATOR_START\n\
000068 GET_ITERATOR\n\
000069 GET_ASYNC_ITERATOR\n\
000070 ITERATOR_NEXT\n\
000071 ITERATOR_CLOSE\n\
000072 ITERATOR_CLOSE_THROW\n\
000073 ARRAY_PUSH\n\
000074 SPREAD_APPEND\n\
000075 CALL_SPREAD\n\
000076 NEW\n\
000077 NEW_SPREAD\n\
000078 SUPER_CONSTRUCT_SPREAD\n\
000079 BIND_THIS_VALUE\n\
000080 LOAD_SUPER_PROPERTY\n\
000081 LOAD_SUPER_ELEMENT\n\
000082 SET_SUPER_PROPERTY\n\
000083 SET_SUPER_ELEMENT\n\
000084 GLOBAL_BINDING_EXISTS\n\
000085 STORE_GLOBAL_CHECKED\n\
000086 MAKE_CLASS\n\
000087 MATH_LOAD\n\
000088 COLLECT_REST\n\
000089 RETURN_VALUE\n\
000090 RETURN_UNDEFINED\n\
000091 RETURN_DERIVED\n\
000092 NEW_OBJECT\n\
000093 LOAD_PROPERTY\n\
000094 STORE_PROPERTY\n\
000095 DELETE_PROPERTY\n\
000096 GET_PROTOTYPE\n\
000097 SET_PROTOTYPE\n\
000098 NEW_ARRAY\n\
000099 LOAD_ELEMENT\n\
000100 STORE_ELEMENT\n\
000101 ARRAY_LENGTH\n\
000102 HAS_PROPERTY\n\
000103 INSTANCEOF\n\
000104 EVAL\n\
000105 IS_EVAL_INTRINSIC\n\
000106 NEW_FUNCTION\n\
000107 LOAD_GLOBAL_THIS\n\
000108 LOAD_GLOBAL_OR_THROW\n\
000109 COLLECT_ARGUMENTS\n\
000110 LOAD_GLOBAL_OR_UNDEFINED\n\
000111 DEFINE_GLOBAL_VAR\n\
000112 IMPORT_META_RESOLVE\n\
000113 IMPORT_NAMESPACE_DYNAMIC\n\
000114 IMPORT_NAMESPACE\n\
000115 IMPORT_NAMESPACE_DEFERRED\n\
000116 EVALUATE_MODULE\n\
000117 MARK_MODULE_EVALUATED\n\
000118 STAR_REEXPORT\n\
000119 MODULE_NAMESPACE_OBJECT\n\
000120 LOAD_IMPORT_BINDING\n\
000121 PROMISE_FULFILLED_OF\n\
000122 TEMPORAL_LOAD\n\
000123 NEW_COLLECTION\n\
000124 NEW_WEAK_REF\n\
000125 NEW_FINALIZATION_REGISTRY\n\
000126 SYMBOL_LOAD\n\
000127 TYPEOF\n\
000128 TEST_TYPEOF\n\
000129 DELETE_ELEMENT\n\
000130 AWAIT\n\
000131 SAME_VALUE\n\
000132 IS_ARRAY\n\
000133 LOOSE_EQ\n\
000134 LOOSE_NEQ\n\
000135 NEW_BUILTIN_ERROR\n\
000136 LOAD_BUILTIN_ERROR\n\
000137 BIGINT_CALL\n\
000138 ARRAY_CONSTRUCT\n\
000139 ARRAY_FROM\n\
000140 ARRAY_OF\n\
000141 ARRAY_BUFFER_CALL\n\
000142 DATA_VIEW_CALL\n\
000143 YIELD\n\
000144 SHARED_ARRAY_BUFFER_CALL\n\
000145 TO_PRIMITIVE\n\
000146 FOR_IN_KEYS\n\
000147 COPY_DATA_PROPERTIES\n\
000148 DEFINE_OWN_PROPERTY\n\
000149 NEW_OBJECT_LITERAL\n\
000150 LOAD_LOOKUP_SLOT\n\
000151 STORE_LOOKUP_SLOT\n\
000152 DELETE_LOOKUP_SLOT\n\
000153 LOAD_LOOKUP_GLOBAL\n\
000154 TYPEOF_LOOKUP_GLOBAL\n\
000155 STORE_LOOKUP_GLOBAL\n\
000156 DELETE_LOOKUP_GLOBAL\n\
000157 RESOLVE_LOOKUP_REF\n\
000158 STORE_REF\n\
000159 DECLARE_EVAL_VAR\n\
000160 STORE_VAR_SCOPE\n";
}
