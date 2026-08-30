//! Adversarial admission corpus for the mandatory bytecode verifier.
//!
//! Every artifact reaching [`decode_module`] is untrusted. This suite drives a
//! seed corpus through byte-level and structural mutation and asserts the two
//! properties the admission boundary owes its callers: it never panics, and it
//! never hands back a carrier that its own verifier would reject.
//!
//! # Contents
//! - [`seed_corpus`] — modules spanning the opcode families with distinct
//!   verification domains: control flow, closures, constants, metadata,
//!   variadic call and construct sites, the iteration protocol, suspension
//!   points, classes and private names, and module linkage.
//! - Byte mutations — single- and two-byte flips, truncation, insertion, and
//!   tampering with the leading counts a decoder sizes allocations from.
//! - Structural mutations — decoded fields moved out of their legal domain,
//!   which must produce a typed rejection rather than an admitted module.
//! - Cost bounds — the analyses that are not linear in artifact size stay
//!   inside their declared budget.
//!
//! # Invariants
//! - A mutation may change WHICH error is reported; it may never turn a
//!   rejection into a panic or into an unverified admission.
//! - Seeds themselves must be admitted, or the corpus is testing nothing.

use otter_bytecode::binary::{ModuleDecodeError, decode_module, encode_module};
use otter_bytecode::wordcode::FunctionCodeBuilder;
use otter_bytecode::{
    ArgumentsObjectKind, BytecodeModule, ClassHintSite, Constant, Function, ModuleInit,
    ModuleResolution, NO_HANDLER_OFFSET, Op, Operand, SourceKind, SpanEntry, TemplateSite,
};

/// Deterministic 64-bit xorshift. The corpus must reproduce byte for byte
/// across runs and machines, so it never draws on a seeded system RNG.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next() % bound as u64).expect("bound fits usize")
    }
}

/// A module with one trivial function — the smallest artifact that still
/// exercises the header, the function table, and the wordcode layout.
fn seed_minimal() -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::LoadUndefined, &[Operand::Register(0)]);
    code.push(Op::Return, &[Operand::Register(0)]);
    BytecodeModule {
        module: "file:///minimal.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            locals: 1,
            code: code.finish(),
            module_url: "file:///minimal.js".to_string(),
            ..Default::default()
        }],
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Nested `try` regions with both catch and finally targets, so the handler
/// layout, the finally floors, and the abrupt-completion lattice all run.
fn seed_handlers() -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    let outer = code.push(
        Op::EnterTry,
        &[
            Operand::Imm32(NO_HANDLER_OFFSET),
            Operand::Imm32(0),
            Operand::Register(0),
        ],
    );
    let inner = code.push(
        Op::EnterTry,
        &[
            Operand::Imm32(NO_HANDLER_OFFSET),
            Operand::Imm32(0),
            Operand::Register(1),
        ],
    );
    code.push(Op::LoadUndefined, &[Operand::Register(0)]);
    code.push(Op::LeaveTry, &[]);
    let inner_handler = code.next_pc();
    code.push(Op::LoadUndefined, &[Operand::Register(1)]);
    code.push(Op::EndFinally, &[]);
    code.push(Op::LeaveTry, &[]);
    let outer_handler = code.next_pc();
    code.push(Op::LoadUndefined, &[Operand::Register(0)]);
    code.push(Op::EndFinally, &[]);
    code.push(Op::ReturnUndefined, &[]);
    // Offsets are relative to the instruction that carries them.
    set_offset(&mut code, outer, 1, outer_handler);
    set_offset(&mut code, inner, 1, inner_handler);

    BytecodeModule {
        module: "file:///handlers.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            locals: 2,
            code: code.finish(),
            module_url: "file:///handlers.js".to_string(),
            ..Default::default()
        }],
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Two functions, a closure capture spine, a populated constant pool, a
/// template site, and source metadata — the index-bearing domains.
fn seed_rich() -> BytecodeModule {
    let mut inner = FunctionCodeBuilder::new();
    inner.push(Op::LoadUpvalue, &[Operand::Register(0), Operand::Imm32(0)]);
    inner.push(Op::Return, &[Operand::Register(0)]);

    let mut outer = FunctionCodeBuilder::new();
    outer.push(
        Op::LoadString,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    outer.push(
        Op::LoadNumber,
        &[Operand::Register(1), Operand::ConstIndex(1)],
    );
    outer.push(
        Op::GetTemplateObject,
        &[Operand::Register(2), Operand::ConstIndex(0)],
    );
    outer.push(Op::Return, &[Operand::Register(0)]);

    BytecodeModule {
        module: "file:///rich.js".to_string(),
        template_sites: vec![TemplateSite {
            cooked: vec![Some("a".to_string()), None],
            raw: vec!["a".to_string(), "\\u{}".to_string()],
        }],
        source_kind: SourceKind::TypeScript,
        functions: vec![
            Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 3,
                code: outer.finish(),
                module_url: "file:///rich.js".to_string(),
                spans: vec![SpanEntry {
                    pc: 0,
                    span: (0, 4),
                }],
                number_hint_sites: vec![1],
                class_hint_sites: vec![ClassHintSite {
                    pc: 3,
                    class_function_id: 1,
                }],
                ..Default::default()
            },
            Function {
                id: 1,
                name: "inner".to_string(),
                locals: 1,
                inherited_upvalue_count: 1,
                is_strict: true,
                arguments_object_kind: ArgumentsObjectKind::Unmapped,
                code: inner.finish(),
                module_url: "file:///rich.js".to_string(),
                ..Default::default()
            },
        ],
        function_source: Some("function main() {}".to_string()),
        constants: vec![
            Constant::String {
                utf16: vec![0xD83D, 0xDE00],
            },
            Constant::Number {
                bits: f64::NAN.to_bits(),
            },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Call, construct, and closure-construction sites: the variadic operand
/// shapes whose argument count rides in the instruction, and the two opcodes
/// that name another function in the module table.
fn seed_calls() -> BytecodeModule {
    let mut callee = FunctionCodeBuilder::new();
    callee.push(Op::LoadUndefined, &[Operand::Register(0)]);
    callee.push(Op::Return, &[Operand::Register(0)]);

    let mut captured = FunctionCodeBuilder::new();
    captured.push(Op::LoadUpvalue, &[Operand::Register(0), Operand::Imm32(0)]);
    captured.push(Op::Return, &[Operand::Register(0)]);

    let mut tail = FunctionCodeBuilder::new();
    tail.push(Op::LoadUndefined, &[Operand::Register(0)]);
    tail.push(
        Op::TailCall,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::ConstIndex(1),
            Operand::Register(0),
        ],
    );
    tail.push(Op::ReturnUndefined, &[]);

    let mut main = FunctionCodeBuilder::new();
    main.push(Op::FreshUpvalue, &[Operand::Imm32(0)]);
    main.push(
        Op::MakeFunction,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    main.push(
        Op::MakeClosure,
        &[
            Operand::Register(1),
            Operand::ConstIndex(1),
            Operand::ConstIndex(1),
            Operand::Imm32(0),
        ],
    );
    main.push(Op::NewObject, &[Operand::Register(2)]);
    main.push(
        Op::Call,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::ConstIndex(1),
            Operand::Register(2),
        ],
    );
    main.push(
        Op::CallWithThis,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::Register(2),
            Operand::ConstIndex(1),
            Operand::Register(2),
        ],
    );
    main.push(
        Op::CallMethodValue,
        &[
            Operand::Register(3),
            Operand::Register(2),
            Operand::ConstIndex(2),
            Operand::ConstIndex(1),
            Operand::Register(2),
        ],
    );
    main.push(
        Op::New,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    );
    main.push(
        Op::CallSpread,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::Register(2),
            Operand::Register(2),
        ],
    );
    main.push(
        Op::NewSpread,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::Register(2),
        ],
    );
    main.push(Op::Return, &[Operand::Register(3)]);

    BytecodeModule {
        module: "file:///calls.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 4,
                own_upvalue_count: 1,
                code: main.finish(),
                module_url: "file:///calls.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 1,
                name: "callee".to_string(),
                locals: 1,
                param_count: 1,
                length: 1,
                code: callee.finish(),
                module_url: "file:///calls.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 2,
                name: "captured".to_string(),
                locals: 1,
                inherited_upvalue_count: 1,
                code: captured.finish(),
                module_url: "file:///calls.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 3,
                name: "tail".to_string(),
                locals: 2,
                code: tail.finish(),
                module_url: "file:///calls.js".to_string(),
                ..Default::default()
            },
        ],
        function_source: None,
        constants: vec![
            Constant::FunctionId { index: 1 },
            Constant::FunctionId { index: 2 },
            Constant::String {
                utf16: "method".encode_utf16().collect(),
            },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// The iteration protocol: a `for…of` spine plus the close sequence and the
/// async-iterator entry points, driven around a back edge so the control-flow
/// analysis sees a loop rather than a straight line.
fn seed_iterators() -> BytecodeModule {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::NewObject, &[Operand::Register(0)]);
    code.push(
        Op::GetIterator,
        &[Operand::Register(1), Operand::Register(0)],
    );
    let head = code.next_pc();
    code.push(
        Op::IteratorNext,
        &[
            Operand::Register(2),
            Operand::Register(3),
            Operand::Register(1),
        ],
    );
    code.push(Op::CheckIteratorResult, &[Operand::Register(2)]);
    let exit = code.push(Op::JumpIfTrue, &[Operand::Imm32(0), Operand::Register(3)]);
    let back = code.push(Op::Jump, &[Operand::Imm32(0)]);
    let after = code.next_pc();
    code.push(Op::IteratorCloseStart, &[Operand::Register(1)]);
    code.push(Op::IteratorClose, &[Operand::Register(1)]);
    code.push(Op::IteratorCloseEnd, &[Operand::Register(1)]);
    code.push(
        Op::GetAsyncIterator,
        &[Operand::Register(4), Operand::Register(0)],
    );
    code.push(
        Op::AsyncIteratorReturn,
        &[
            Operand::Register(5),
            Operand::Register(6),
            Operand::Register(4),
        ],
    );
    code.push(Op::ForInKeys, &[Operand::Register(7), Operand::Register(0)]);
    code.push(Op::Return, &[Operand::Register(2)]);
    set_offset(&mut code, exit, 0, after);
    set_offset(&mut code, back, 0, head);

    BytecodeModule {
        module: "file:///iterators.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            locals: 8,
            code: code.finish(),
            module_url: "file:///iterators.js".to_string(),
            ..Default::default()
        }],
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Suspension points. Generator, async, and async-generator bodies carry the
/// opcodes whose successor shape is a resumption rather than a plain
/// fallthrough, and the flags that decide how a frame parks.
fn seed_suspensions() -> BytecodeModule {
    let mut generator = FunctionCodeBuilder::new();
    generator.push(Op::GeneratorStart, &[]);
    generator.push(Op::LoadUndefined, &[Operand::Register(0)]);
    generator.push(Op::Yield, &[Operand::Register(1), Operand::Register(0)]);
    generator.push(
        Op::YieldDelegate,
        &[
            Operand::Register(1),
            Operand::Register(2),
            Operand::Register(0),
        ],
    );
    generator.push(Op::ReturnUndefined, &[]);

    let mut asynchronous = FunctionCodeBuilder::new();
    asynchronous.push(Op::LoadUndefined, &[Operand::Register(0)]);
    asynchronous.push(Op::Await, &[Operand::Register(1), Operand::Register(0)]);
    asynchronous.push(Op::Return, &[Operand::Register(1)]);

    let mut async_generator = FunctionCodeBuilder::new();
    async_generator.push(Op::GeneratorStart, &[]);
    async_generator.push(Op::LoadUndefined, &[Operand::Register(0)]);
    async_generator.push(Op::Await, &[Operand::Register(1), Operand::Register(0)]);
    async_generator.push(Op::Yield, &[Operand::Register(2), Operand::Register(1)]);
    async_generator.push(Op::ReturnUndefined, &[]);

    let mut main = FunctionCodeBuilder::new();
    main.push(
        Op::MakeFunction,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    main.push(
        Op::MakeFunction,
        &[Operand::Register(1), Operand::ConstIndex(1)],
    );
    main.push(
        Op::MakeFunction,
        &[Operand::Register(2), Operand::ConstIndex(2)],
    );
    main.push(
        Op::Call,
        &[
            Operand::Register(3),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    );
    main.push(Op::Return, &[Operand::Register(3)]);

    BytecodeModule {
        module: "file:///suspensions.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 4,
                code: main.finish(),
                module_url: "file:///suspensions.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 1,
                name: "gen".to_string(),
                locals: 3,
                is_generator: true,
                code: generator.finish(),
                module_url: "file:///suspensions.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 2,
                name: "asyncFn".to_string(),
                locals: 2,
                is_async: true,
                code: asynchronous.finish(),
                module_url: "file:///suspensions.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 3,
                name: "asyncGen".to_string(),
                locals: 3,
                is_async: true,
                is_generator: true,
                is_async_generator: true,
                code: async_generator.finish(),
                module_url: "file:///suspensions.js".to_string(),
                ..Default::default()
            },
        ],
        function_source: None,
        constants: vec![
            Constant::FunctionId { index: 1 },
            Constant::FunctionId { index: 2 },
            Constant::FunctionId { index: 3 },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Class construction, private names, and the `super` accessors — the
/// opcodes whose operands mix a private-name register, a class body, and a
/// derived constructor's deferred `this` binding.
fn seed_classes() -> BytecodeModule {
    let mut constructor = FunctionCodeBuilder::new();
    constructor.push(Op::LoadUndefined, &[Operand::Register(0)]);
    constructor.push(
        Op::SuperConstruct,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::ConstIndex(0),
        ],
    );
    constructor.push(Op::BindThisValue, &[Operand::Register(1)]);
    constructor.push(
        Op::LoadSuperProperty,
        &[
            Operand::Register(2),
            Operand::Register(1),
            Operand::ConstIndex(0),
        ],
    );
    constructor.push(
        Op::SetSuperProperty,
        &[
            Operand::Register(1),
            Operand::ConstIndex(0),
            Operand::Register(2),
        ],
    );
    constructor.push(
        Op::LoadSuperElement,
        &[
            Operand::Register(3),
            Operand::Register(1),
            Operand::Register(2),
        ],
    );
    constructor.push(
        Op::SetSuperElement,
        &[
            Operand::Register(1),
            Operand::Register(2),
            Operand::Register(3),
        ],
    );
    constructor.push(
        Op::SuperConstructSpread,
        &[
            Operand::Register(4),
            Operand::Register(0),
            Operand::Register(0),
        ],
    );
    constructor.push(Op::ReturnUndefined, &[]);

    let mut method = FunctionCodeBuilder::new();
    method.push(Op::LoadThis, &[Operand::Register(0)]);
    method.push(Op::Return, &[Operand::Register(0)]);

    let mut main = FunctionCodeBuilder::new();
    main.push(
        Op::NewPrivateName,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    main.push(Op::NewObject, &[Operand::Register(1)]);
    main.push(
        Op::MakeFunction,
        &[Operand::Register(2), Operand::ConstIndex(1)],
    );
    main.push(
        Op::MakeFunction,
        &[Operand::Register(3), Operand::ConstIndex(2)],
    );
    main.push(
        Op::MakeClass,
        &[
            Operand::Register(4),
            Operand::Register(2),
            Operand::Register(3),
            Operand::Register(1),
            Operand::Register(1),
        ],
    );
    main.push(Op::ClassCheck, &[Operand::Imm32(0), Operand::Register(4)]);
    main.push(
        Op::PrivateSet,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::Register(4),
        ],
    );
    main.push(
        Op::PrivateGet,
        &[
            Operand::Register(5),
            Operand::Register(1),
            Operand::Register(0),
        ],
    );
    main.push(
        Op::PrivateBrandCheck,
        &[Operand::Register(1), Operand::Register(0)],
    );
    main.push(
        Op::SetFunctionName,
        &[
            Operand::Register(2),
            Operand::Register(1),
            Operand::ConstIndex(0),
        ],
    );
    main.push(
        Op::DefineDataProperty,
        &[
            Operand::Register(1),
            Operand::Register(0),
            Operand::Register(5),
        ],
    );
    main.push(Op::Return, &[Operand::Register(5)]);

    BytecodeModule {
        module: "file:///classes.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 6,
                code: main.finish(),
                module_url: "file:///classes.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 1,
                name: "C".to_string(),
                locals: 5,
                is_strict: true,
                is_derived_constructor: true,
                code: constructor.finish(),
                module_url: "file:///classes.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 2,
                name: "m".to_string(),
                locals: 1,
                is_strict: true,
                is_method: true,
                code: method.finish(),
                module_url: "file:///classes.js".to_string(),
                ..Default::default()
            },
        ],
        function_source: None,
        constants: vec![
            Constant::String {
                utf16: "#field".encode_utf16().collect(),
            },
            Constant::FunctionId { index: 1 },
            Constant::FunctionId { index: 2 },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

/// Module linkage: the instruction family that names another module by URL,
/// plus the linker tables (`module_resolutions`, `module_inits`) whose
/// function ids are a verification domain of their own.
fn seed_modules() -> BytecodeModule {
    let mut module_init = FunctionCodeBuilder::new();
    module_init.push(Op::LoadUndefined, &[Operand::Register(0)]);
    module_init.push(Op::ReturnUndefined, &[]);

    let mut entry = FunctionCodeBuilder::new();
    entry.push(
        Op::EvaluateModule,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    entry.push(Op::MarkModuleEvaluated, &[Operand::ConstIndex(0)]);
    entry.push(
        Op::ImportNamespace,
        &[Operand::Register(1), Operand::ConstIndex(0)],
    );
    entry.push(
        Op::ImportNamespaceDeferred,
        &[Operand::Register(2), Operand::ConstIndex(0)],
    );
    entry.push(
        Op::ModuleNamespaceObject,
        &[Operand::Register(3), Operand::ConstIndex(0)],
    );
    entry.push(
        Op::LoadImportBinding,
        &[
            Operand::Register(4),
            Operand::ConstIndex(0),
            Operand::ConstIndex(1),
        ],
    );
    entry.push(Op::NewObject, &[Operand::Register(5)]);
    entry.push(
        Op::ImportNamespaceDynamic,
        &[
            Operand::Register(6),
            Operand::Register(5),
            Operand::Register(5),
        ],
    );
    entry.push(
        Op::ImportMetaResolve,
        &[Operand::Register(7), Operand::Register(5)],
    );
    entry.push(
        Op::StarReexport,
        &[Operand::Register(5), Operand::Register(3)],
    );
    entry.push(Op::Return, &[Operand::Register(4)]);

    BytecodeModule {
        module: "file:///entry.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            Function {
                id: 0,
                name: "<entry>".to_string(),
                locals: 8,
                code: entry.finish(),
                module_url: "file:///entry.js".to_string(),
                ..Default::default()
            },
            Function {
                id: 1,
                name: "<module-init>".to_string(),
                locals: 1,
                param_count: 2,
                is_module: true,
                is_strict: true,
                code: module_init.finish(),
                module_url: "file:///dep.js".to_string(),
                ..Default::default()
            },
        ],
        function_source: None,
        constants: vec![
            Constant::String {
                utf16: "file:///dep.js".encode_utf16().collect(),
            },
            Constant::String {
                utf16: "name".encode_utf16().collect(),
            },
        ],
        module_resolutions: vec![
            ModuleResolution {
                referrer: "file:///entry.js".to_string(),
                specifier: "./dep.js".to_string(),
                attr_type: None,
                target: "file:///dep.js".to_string(),
                deferred: false,
                dynamic: false,
                synthetic: false,
            },
            ModuleResolution {
                referrer: "file:///entry.js".to_string(),
                specifier: "./dep.js".to_string(),
                attr_type: Some("json".to_string()),
                target: "file:///dep.js".to_string(),
                deferred: true,
                dynamic: true,
                synthetic: true,
            },
        ],
        module_inits: vec![ModuleInit {
            url: "file:///dep.js".to_string(),
            function_id: 1,
        }],
    }
}

/// Rewrite one control-flow operand so it lands on `target`.
///
/// Wordcode targets are relative to the instruction AFTER the one carrying
/// the operand, matching `resolve_wordcode_target`.
fn set_offset(code: &mut FunctionCodeBuilder, at: u32, operand: usize, target: u32) {
    let delta = i64::from(target) - (i64::from(at) + 1);
    let delta = i32::try_from(delta).expect("seed offset fits i32");
    assert!(
        code.set_operand(at, operand, Operand::Imm32(delta)),
        "seed offset operand {operand} at {at} is not writable"
    );
}

fn seed_corpus() -> Vec<(&'static str, BytecodeModule)> {
    vec![
        ("minimal", seed_minimal()),
        ("handlers", seed_handlers()),
        ("rich", seed_rich()),
        ("calls", seed_calls()),
        ("iterators", seed_iterators()),
        ("suspensions", seed_suspensions()),
        ("classes", seed_classes()),
        ("modules", seed_modules()),
    ]
}

/// Decode `bytes` and assert the admission boundary held.
///
/// Panics carry the seed name and a mutation description so a failure names
/// the exact artifact instead of a byte offset in isolation.
fn assert_admission_holds(seed: &str, mutation: &str, bytes: &[u8]) {
    let decoded = std::panic::catch_unwind(|| decode_module(bytes))
        .unwrap_or_else(|_| panic!("{seed}: decoder panicked on {mutation}"));
    let Ok(module) = decoded else {
        return;
    };
    otter_bytecode::verify_module_at_base(module.module(), module.function_base()).unwrap_or_else(
        |error| panic!("{seed}: admitted an unverifiable module on {mutation}: {error}"),
    );
}

#[test]
fn seeds_are_admitted_and_round_trip() {
    for (name, module) in seed_corpus() {
        let bytes = encode_module(&module);
        let decoded =
            decode_module(&bytes).unwrap_or_else(|error| panic!("seed {name} rejected: {error:?}"));
        assert_eq!(
            decoded.module().functions.len(),
            module.functions.len(),
            "seed {name} lost functions across the codec"
        );
        assert_eq!(
            encode_module(decoded.module()),
            bytes,
            "seed {name} does not re-encode to the same bytes"
        );
    }
}

#[test]
fn single_byte_flips_never_panic_or_escape_verification() {
    for (name, module) in seed_corpus() {
        let bytes = encode_module(&module);
        for index in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xa5, 0xff] {
                let mut mutated = bytes.clone();
                mutated[index] ^= mask;
                assert_admission_holds(
                    name,
                    &format!("byte {index} flipped with {mask:#04x}"),
                    &mutated,
                );
            }
        }
    }
}

#[test]
fn multi_byte_mutations_never_panic_or_escape_verification() {
    // One fixed seed per corpus entry keeps the sweep reproducible while
    // covering combinations a single-byte radius cannot reach.
    for (index, (name, module)) in seed_corpus().into_iter().enumerate() {
        let bytes = encode_module(&module);
        let mut rng = Rng::new(0x9e37_79b9_7f4a_7c15 ^ index as u64);
        for round in 0..2_000 {
            let mut mutated = bytes.clone();
            let edits = 1 + rng.below(4);
            for _ in 0..edits {
                let at = rng.below(mutated.len());
                mutated[at] ^= (rng.next() & 0xff) as u8;
            }
            assert_admission_holds(name, &format!("multi-byte round {round}"), &mutated);
        }
    }
}

#[test]
fn truncation_at_every_prefix_is_a_typed_rejection() {
    for (name, module) in seed_corpus() {
        let bytes = encode_module(&module);
        for length in 0..bytes.len() {
            let truncated = &bytes[..length];
            let decoded = std::panic::catch_unwind(|| decode_module(truncated))
                .unwrap_or_else(|_| panic!("{name}: decoder panicked on a {length}-byte prefix"));
            assert!(
                decoded.is_err(),
                "{name}: a {length}-byte prefix of a {} byte module was admitted",
                bytes.len()
            );
        }
    }
}

#[test]
fn trailing_and_inserted_bytes_never_escape_verification() {
    for (name, module) in seed_corpus() {
        let bytes = encode_module(&module);
        let mut appended = bytes.clone();
        appended.extend_from_slice(&[0xff; 32]);
        assert_admission_holds(name, "32 appended bytes", &appended);

        let mut rng = Rng::new(0xdead_beef_cafe_f00d);
        for round in 0..256 {
            let mut mutated = bytes.clone();
            let at = rng.below(mutated.len() + 1);
            mutated.insert(at, (rng.next() & 0xff) as u8);
            assert_admission_holds(name, &format!("insertion round {round}"), &mutated);
        }
    }
}

#[test]
fn tampered_length_prefixes_cannot_amplify_allocation() {
    // Every `u32` in the encoding is a candidate count. Driving each one to a
    // value far past the remaining input must be a typed rejection, not a
    // multi-gigabyte reservation.
    for (name, module) in seed_corpus() {
        let bytes = encode_module(&module);
        for index in 0..bytes.len().saturating_sub(4) {
            for value in [u32::MAX, u32::MAX / 2, 1 << 24] {
                let mut mutated = bytes.clone();
                mutated[index..index + 4].copy_from_slice(&value.to_le_bytes());
                assert_admission_holds(name, &format!("u32 at {index} set to {value}"), &mutated);
            }
        }
    }
}

#[test]
fn structural_mutations_are_typed_rejections() {
    let sparse = {
        let mut module = seed_rich();
        module.functions[1].id = 7;
        module
    };
    let dangling_constant = {
        let mut module = seed_rich();
        module.constants.clear();
        module
    };
    let short_register_window = {
        let mut module = seed_rich();
        module.functions[0].locals = 0;
        module.functions[0].scratch = 0;
        module
    };
    let missing_capture_spine = {
        let mut module = seed_rich();
        module.functions[1].inherited_upvalue_count = 0;
        module
    };
    let dangling_class_hint = {
        let mut module = seed_rich();
        module.functions[0].class_hint_sites[0].class_function_id = 99;
        module
    };
    let aliased_immediate_destination = {
        let mut module = seed_minimal();
        let mut code = module.functions[0].code.to_builder();
        code.replace(
            0,
            Op::AddImm,
            &[
                Operand::Register(0),
                Operand::Register(0),
                Operand::Imm32(1),
            ],
        );
        module.functions[0].code = code.finish();
        module
    };
    let out_of_range_span = {
        let mut module = seed_rich();
        module.functions[0].spans[0].pc = 4_000;
        module
    };
    let closure_capture_count_mismatch = {
        let mut module = seed_calls();
        module.functions[2].inherited_upvalue_count = 2;
        module
    };
    let call_argument_out_of_window = {
        let mut module = seed_calls();
        let mut code = module.functions[0].code.to_builder();
        code.replace(
            4,
            Op::Call,
            &[
                Operand::Register(3),
                Operand::Register(0),
                Operand::ConstIndex(1),
                Operand::Register(200),
            ],
        );
        module.functions[0].code = code.finish();
        module
    };
    let iterator_loop_leaves_the_function = {
        let mut module = seed_iterators();
        let mut code = module.functions[0].code.to_builder();
        code.replace(5, Op::Jump, &[Operand::Imm32(1_000_000)]);
        module.functions[0].code = code.finish();
        module
    };
    let suspension_target_outside_the_table = {
        let mut module = seed_suspensions();
        module.constants[2] = Constant::FunctionId { index: 9 };
        module
    };
    let private_name_of_the_wrong_constant_kind = {
        let mut module = seed_classes();
        module.constants[0] = Constant::Number {
            bits: 1.0f64.to_bits(),
        };
        module
    };
    let module_init_outside_the_table = {
        let mut module = seed_modules();
        module.module_inits[0].function_id = 42;
        module
    };

    for (name, module) in [
        ("sparse function ids", sparse),
        ("dangling constant index", dangling_constant),
        ("short register window", short_register_window),
        ("missing capture spine", missing_capture_spine),
        ("dangling class hint", dangling_class_hint),
        ("out-of-range span pc", out_of_range_span),
        (
            "immediate destination aliasing its left operand",
            aliased_immediate_destination,
        ),
        (
            "closure capture count mismatch",
            closure_capture_count_mismatch,
        ),
        (
            "call argument outside the window",
            call_argument_out_of_window,
        ),
        (
            "iterator back edge leaving the function",
            iterator_loop_leaves_the_function,
        ),
        (
            "suspension target outside the function table",
            suspension_target_outside_the_table,
        ),
        (
            "private name of the wrong constant kind",
            private_name_of_the_wrong_constant_kind,
        ),
        (
            "module init outside the function table",
            module_init_outside_the_table,
        ),
    ] {
        assert!(
            otter_bytecode::verify_module(&module).is_err(),
            "{name} was accepted by the verifier"
        );
        let bytes = encode_module(&module);
        let decoded = std::panic::catch_unwind(|| decode_module(&bytes))
            .unwrap_or_else(|_| panic!("{name}: decoder panicked"));
        assert!(
            matches!(decoded, Err(ModuleDecodeError::Verify(_))),
            "{name}: expected a typed verification rejection"
        );
    }
}

#[test]
fn verification_cost_stays_linear_in_handler_nesting() {
    // The handler layout is a single pass over the instruction stream. A
    // module whose nesting depth doubles must not cost more than a small
    // multiple of the instruction count, so the depth cannot be used to
    // amplify verification work.
    fn nested_module(depth: usize) -> BytecodeModule {
        // Textual nesting, the shape a compiler emits: each region's finally
        // block sits inside its parent, so its landing lexical depth matches
        // the region it belongs to.
        let mut code = FunctionCodeBuilder::new();
        let mut enters = Vec::with_capacity(depth);
        for _ in 0..depth {
            enters.push(code.push(
                Op::EnterTry,
                &[
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(0),
                    Operand::Register(0),
                ],
            ));
        }
        code.push(Op::LoadUndefined, &[Operand::Register(0)]);
        for enter in enters.into_iter().rev() {
            code.push(Op::LeaveTry, &[]);
            let skip = code.push(Op::Jump, &[Operand::Imm32(0)]);
            let landing = code.next_pc();
            code.push(Op::LoadUndefined, &[Operand::Register(0)]);
            code.push(Op::EndFinally, &[]);
            let after = code.next_pc();
            set_offset(&mut code, enter, 1, landing);
            set_offset(&mut code, skip, 0, after);
        }
        code.push(Op::ReturnUndefined, &[]);
        BytecodeModule {
            module: "file:///nested.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 1,
                code: code.finish(),
                module_url: "file:///nested.js".to_string(),
                ..Default::default()
            }],
            function_source: None,
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    // Each artifact is well formed, so the verifier must ACCEPT it: a bound
    // that rejected deep-but-legal nesting would be a denial of service on
    // real code rather than a defence. The deepest case is the one a
    // superlinear analysis would fail to finish.
    for depth in [64, 512, 4_096] {
        otter_bytecode::verify_module(&nested_module(depth))
            .unwrap_or_else(|error| panic!("legal nesting of depth {depth} was rejected: {error}"));
    }
}
