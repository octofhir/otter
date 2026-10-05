//! Interpreter-level context semantics over hand-written bytecode.

use super::*;
use crate::run_control::{ErrorDetail, RunError};
use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction,
    MappedArgumentBinding, Op, Operand, ScopeFlags, ScopeKind, SlotDescriptor, SourceKind,
    SpanEntry,
};

fn coord(depth: u16, slot: u16) -> Operand {
    Operand::Imm32(
        ContextCoord::new(depth, slot)
            .expect("coordinate")
            .to_imm32(),
    )
}

fn string(text: &str) -> Constant {
    Constant::String {
        utf16: text.encode_utf16().collect(),
    }
}

fn scope(kind: ScopeKind, flags: ScopeFlags, slots: &[(&str, SlotKind)]) -> ScopeDescriptor {
    ScopeDescriptor {
        kind,
        flags,
        slots: slots
            .iter()
            .map(|(name, kind)| SlotDescriptor {
                name: (*name).to_string(),
                kind: *kind,
                exported: false,
            })
            .collect(),
    }
}

fn plain(kind: ScopeKind, slots: &[(&str, SlotKind)]) -> ScopeDescriptor {
    scope(kind, ScopeFlags::default(), slots)
}

fn eval_var_scope(slots: &[(&str, SlotKind)]) -> ScopeDescriptor {
    scope(
        ScopeKind::Body,
        ScopeFlags {
            strict: false,
            var_scope: true,
            has_extension: true,
        },
        slots,
    )
}

/// A function over `code` with PCs assigned in order and a spare register
/// window of `locals`.
fn function(
    id: u32,
    locals: u16,
    scopes: Vec<ScopeDescriptor>,
    code: Vec<(Op, Vec<Operand>)>,
) -> Function {
    let code: Vec<Instruction> = code
        .into_iter()
        .enumerate()
        .map(|(pc, (op, operands))| Instruction {
            pc: pc as u32,
            op,
            operands,
        })
        .collect();
    let spans = code
        .iter()
        .map(|instruction| SpanEntry {
            pc: instruction.pc,
            span: (0, 0),
        })
        .collect();
    Function {
        id,
        name: format!("f{id}"),
        locals,
        scopes,
        code: code.into(),
        spans,
        ..Function::default()
    }
}

fn module(functions: Vec<Function>, constants: Vec<Constant>) -> BytecodeModule {
    BytecodeModule {
        module: "context-ops-test.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions,
        constants,
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
        function_source: None,
    }
}

fn run_on(interp: &mut Interpreter, module: BytecodeModule) -> Result<Value, RunError> {
    let context = interp
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified fixture");
    interp.run(&context)
}

fn run(module: BytecodeModule) -> Result<Value, RunError> {
    run_on(
        &mut Interpreter::new().expect("fixture interpreter bootstrap"),
        module,
    )
}

fn error_text(error: &RunError) -> String {
    match &error.detail {
        Some(
            ErrorDetail::Message(text) | ErrorDetail::Uncaught(text) | ErrorDetail::Name(text),
        ) => text.to_string(),
        other => format!("{other:?}"),
    }
}

fn int(value: Value) -> i32 {
    value.as_i32().unwrap_or_else(|| {
        value
            .as_number()
            .map(|number| number.as_f64() as i32)
            .expect("numeric result")
    })
}

/// `main` creates a context holding `x = 41` and calls a closure over it that
/// increments the slot; the result sums the closure's return and the slot.
fn closure_counter_module() -> BytecodeModule {
    let main = function(
        0,
        6,
        vec![plain(ScopeKind::Body, &[("x", SlotKind::Let)])],
        vec![
            (Op::LoadUndefined, vec![Operand::Register(0)]),
            (
                Op::CreateContext,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(41)],
            ),
            (
                Op::StoreContextSlot,
                vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
            ),
            (
                Op::MakeClosure,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Call,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadContextSlotChecked,
                vec![Operand::Register(5), Operand::Register(1), coord(0, 0)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(5),
                    Operand::Register(5),
                    Operand::Register(4),
                ],
            ),
            (Op::Return, vec![Operand::Register(5)]),
        ],
    );
    let increment = function(
        1,
        3,
        Vec::new(),
        vec![
            (Op::LoadClosureContext, vec![Operand::Register(0)]),
            (
                Op::LoadContextSlotChecked,
                vec![Operand::Register(1), Operand::Register(0), coord(0, 0)],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::StoreContextSlotChecked,
                vec![Operand::Register(1), Operand::Register(0), coord(0, 0)],
            ),
            (Op::Return, vec![Operand::Register(1)]),
        ],
    );
    module(
        vec![main, increment],
        vec![Constant::FunctionId { index: 1 }],
    )
}

#[test]
fn a_closure_updates_its_captured_slot_through_its_context() {
    let result = run(closure_counter_module()).expect("program runs");
    assert_eq!(int(result), 84);
}

#[test]
fn contexts_survive_a_scavenge_or_full_collection_at_every_allocation() {
    for full in [false, true] {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        interp.gc_heap_mut().set_gc_stress(1, full);
        let result = run_on(&mut interp, closure_counter_module()).expect("program runs");
        interp.gc_heap_mut().set_gc_stress(0, false);
        assert_eq!(int(result), 84, "full collections: {full}");
    }
}

#[test]
fn a_checked_read_names_the_binding_in_its_tdz_error() {
    for op in [Op::LoadContextSlotChecked, Op::StoreContextSlotChecked] {
        let main = function(
            0,
            3,
            vec![plain(ScopeKind::Block, &[("answer", SlotKind::Let)])],
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (
                    Op::CreateContext,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Imm32(0),
                    ],
                ),
                (
                    op,
                    vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
                ),
                (Op::Return, vec![Operand::Register(2)]),
            ],
        );
        let error = run(module(vec![main], Vec::new())).expect_err("TDZ read throws");
        assert!(
            error_text(&error).contains("Cannot access 'answer' before initialization"),
            "{op:?}: {}",
            error_text(&error)
        );
    }
}

#[test]
fn a_parent_hop_reaches_the_enclosing_scope() {
    let main = function(
        0,
        5,
        vec![
            plain(ScopeKind::Body, &[("a", SlotKind::Var)]),
            plain(ScopeKind::Block, &[("b", SlotKind::Let)]),
        ],
        vec![
            (Op::LoadUndefined, vec![Operand::Register(0)]),
            (
                Op::CreateContext,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(0),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(5)]),
            (
                Op::StoreContextSlot,
                vec![Operand::Register(3), Operand::Register(1), coord(0, 0)],
            ),
            (
                Op::CreateContext,
                vec![
                    Operand::Register(2),
                    Operand::Register(1),
                    Operand::Imm32(1),
                ],
            ),
            (
                Op::LoadContextSlot,
                vec![Operand::Register(4), Operand::Register(2), coord(1, 0)],
            ),
            (Op::Return, vec![Operand::Register(4)]),
        ],
    );
    assert_eq!(int(run(module(vec![main], Vec::new())).unwrap()), 5);
}

#[test]
fn copy_context_gives_each_iteration_its_own_binding() {
    let main = function(
        0,
        7,
        vec![plain(ScopeKind::ForHead, &[("i", SlotKind::Let)])],
        vec![
            (Op::LoadUndefined, vec![Operand::Register(0)]),
            (
                Op::CreateContext,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(0),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
            (
                Op::StoreContextSlot,
                vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
            ),
            (
                Op::MakeClosure,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::CopyContext,
                vec![Operand::Register(1), Operand::Register(1)],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
            (
                Op::StoreContextSlotChecked,
                vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
            ),
            (
                Op::MakeClosure,
                vec![
                    Operand::Register(4),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Call,
                vec![
                    Operand::Register(5),
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::Call,
                vec![
                    Operand::Register(6),
                    Operand::Register(4),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(10)],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(5),
                    Operand::Register(5),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(5),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            (Op::Return, vec![Operand::Register(5)]),
        ],
    );
    let read = function(
        1,
        2,
        Vec::new(),
        vec![
            (Op::LoadClosureContext, vec![Operand::Register(0)]),
            (
                Op::LoadContextSlotChecked,
                vec![Operand::Register(1), Operand::Register(0), coord(0, 0)],
            ),
            (Op::Return, vec![Operand::Register(1)]),
        ],
    );
    let result = run(module(
        vec![main, read],
        vec![Constant::FunctionId { index: 1 }],
    ))
    .expect("program runs");
    assert_eq!(int(result), 12);
}

fn derived_this_program(ops: Vec<(Op, Vec<Operand>)>) -> BytecodeModule {
    let mut code = vec![
        (Op::LoadUndefined, vec![Operand::Register(0)]),
        (
            Op::CreateContext,
            vec![
                Operand::Register(1),
                Operand::Register(0),
                Operand::Imm32(0),
            ],
        ),
    ];
    code.extend(ops);
    let main = function(
        0,
        5,
        vec![plain(ScopeKind::Params, &[("this", SlotKind::DerivedThis)])],
        code,
    );
    module(vec![main], Vec::new())
}

#[test]
fn bind_this_context_slot_rejects_a_second_super_call() {
    let bind = |_: ()| {
        (
            Op::BindThisContextSlot,
            vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
        )
    };
    let program = derived_this_program(vec![
        (Op::NewObject, vec![Operand::Register(2)]),
        bind(()),
        bind(()),
        (Op::ReturnUndefined, Vec::new()),
    ]);
    let error = run(program).expect_err("second bind throws");
    assert!(
        error_text(&error).contains(SUPER_CALLED_TWICE),
        "{}",
        error_text(&error)
    );

    let unbound = derived_this_program(vec![
        (
            Op::LoadContextSlotChecked,
            vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
        ),
        (Op::Return, vec![Operand::Register(2)]),
    ]);
    let error = run(unbound).expect_err("unbound this throws");
    assert!(
        error_text(&error).contains(DERIVED_THIS_UNINITIALIZED),
        "{}",
        error_text(&error)
    );

    let bound = derived_this_program(vec![
        (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
        bind(()),
        (
            Op::LoadContextSlotChecked,
            vec![Operand::Register(3), Operand::Register(1), coord(0, 0)],
        ),
        (Op::Return, vec![Operand::Register(3)]),
    ]);
    assert_eq!(int(run(bound).expect("bound this reads")), 7);
}

/// `main` constructs `f1`, a derived constructor whose `this` lives in a
/// context slot and whose completion is `ReturnDerived value, ctx, coord`.
fn derived_constructor_module(body: Vec<(Op, Vec<Operand>)>) -> BytecodeModule {
    let main = function(
        0,
        3,
        Vec::new(),
        vec![
            (
                Op::MakeFunction,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (
                Op::New,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::Return, vec![Operand::Register(1)]),
        ],
    );
    let mut code = vec![
        (Op::LoadUndefined, vec![Operand::Register(0)]),
        (
            Op::CreateContext,
            vec![
                Operand::Register(1),
                Operand::Register(0),
                Operand::Imm32(0),
            ],
        ),
    ];
    code.extend(body);
    let mut constructor = function(
        1,
        6,
        vec![plain(ScopeKind::Params, &[("this", SlotKind::DerivedThis)])],
        code,
    );
    constructor.is_derived_constructor = true;
    module(
        vec![main, constructor],
        vec![Constant::FunctionId { index: 1 }],
    )
}

fn raw_this_and_return(value: Operand) -> Vec<(Op, Vec<Operand>)> {
    vec![(
        Op::ReturnDerived,
        vec![value, Operand::Register(1), coord(0, 0)],
    )]
}

#[test]
fn return_derived_yields_the_bound_this_an_object_or_an_error() {
    // `undefined` completion after super(): the bound `this`.
    let mut body = vec![
        (Op::NewObject, vec![Operand::Register(2)]),
        (
            Op::BindThisContextSlot,
            vec![Operand::Register(2), Operand::Register(1), coord(0, 0)],
        ),
        (Op::LoadUndefined, vec![Operand::Register(4)]),
    ];
    body.extend(raw_this_and_return(Operand::Register(4)));
    // Heap values are inspected while their interpreter is alive.
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    let result =
        run_on(&mut interp, derived_constructor_module(body)).expect("construction completes");
    assert!(result.is_object(), "{result:?}");

    // An object completion wins even without super().
    let mut body = vec![(Op::NewObject, vec![Operand::Register(4)])];
    body.extend(raw_this_and_return(Operand::Register(4)));
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    let result = run_on(&mut interp, derived_constructor_module(body)).expect("object completion");
    assert!(result.is_object(), "{result:?}");

    // `undefined` completion without super(): ReferenceError.
    let mut body = vec![(Op::LoadUndefined, vec![Operand::Register(4)])];
    body.extend(raw_this_and_return(Operand::Register(4)));
    let error = run(derived_constructor_module(body)).expect_err("unbound this");
    assert!(
        error_text(&error).contains(DERIVED_THIS_UNINITIALIZED),
        "{}",
        error_text(&error)
    );

    // A primitive completion: TypeError.
    let mut body = vec![(Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(3)])];
    body.extend(raw_this_and_return(Operand::Register(4)));
    let error = run(derived_constructor_module(body)).expect_err("primitive completion");
    assert!(
        error_text(&error).contains("derived constructors may only return an object or undefined"),
        "{}",
        error_text(&error)
    );
}

/// `f1(a)` binds its mapped formal into slot 0 of its parameter context in
/// r1, collects a mapped `arguments`, then exercises aliasing both ways.
fn mapped_arguments_module(body: Vec<(Op, Vec<Operand>)>) -> BytecodeModule {
    let main = function(
        0,
        3,
        Vec::new(),
        vec![
            (
                Op::MakeFunction,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(5)]),
            (
                Op::Call,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::Return, vec![Operand::Register(2)]),
        ],
    );
    let mut code = vec![
        (Op::LoadUndefined, vec![Operand::Register(2)]),
        (
            Op::CreateContext,
            vec![
                Operand::Register(1),
                Operand::Register(2),
                Operand::Imm32(0),
            ],
        ),
        (
            Op::StoreContextSlot,
            vec![Operand::Register(0), Operand::Register(1), coord(0, 0)],
        ),
        (
            Op::CollectArguments,
            vec![Operand::Register(3), Operand::Register(1)],
        ),
        (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(0)]),
    ];
    code.extend(body);
    let mut callee = function(
        1,
        7,
        vec![plain(
            ScopeKind::Params,
            &[("a", SlotKind::Param { checked: false })],
        )],
        code,
    );
    callee.param_count = 1;
    callee.length = 1;
    callee.needs_arguments = true;
    callee.arguments_object_kind = ArgumentsObjectKind::Mapped;
    callee.mapped_argument_bindings = vec![MappedArgumentBinding {
        argument_index: 0,
        formal_name: "a".to_string(),
        storage: ArgumentBindingStorage::Context { reg: 1, slot: 0 },
    }];
    module(vec![main, callee], vec![Constant::FunctionId { index: 1 }])
}

#[test]
fn a_mapped_arguments_object_aliases_its_parameter_context_slot() {
    // Writing the formal is visible through `arguments[0]`.
    let program = mapped_arguments_module(vec![
        (
            Op::LoadInt32,
            vec![Operand::Register(4), Operand::Imm32(99)],
        ),
        (
            Op::StoreContextSlotChecked,
            vec![Operand::Register(4), Operand::Register(1), coord(0, 0)],
        ),
        (
            Op::LoadElement,
            vec![
                Operand::Register(6),
                Operand::Register(3),
                Operand::Register(5),
            ],
        ),
        (Op::Return, vec![Operand::Register(6)]),
    ]);
    assert_eq!(int(run(program).expect("aliased read")), 99);

    // Writing `arguments[0]` is visible through the formal.
    let program = mapped_arguments_module(vec![
        (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(7)]),
        (
            Op::StoreElement,
            vec![
                Operand::Register(3),
                Operand::Register(5),
                Operand::Register(4),
            ],
        ),
        (
            Op::LoadContextSlotChecked,
            vec![Operand::Register(6), Operand::Register(1), coord(0, 0)],
        ),
        (Op::Return, vec![Operand::Register(6)]),
    ]);
    assert_eq!(int(run(program).expect("aliased write")), 7);
}

/// Two nested contexts: an outer body scope with slot `v` (r1) and an inner
/// eval var scope with an extension anchor (r2). Constant 0 is `"v"`,
/// constant 1 is `"g"`.
fn lookup_module(body: Vec<(Op, Vec<Operand>)>) -> BytecodeModule {
    let mut code = vec![
        (Op::LoadUndefined, vec![Operand::Register(0)]),
        (
            Op::CreateContext,
            vec![
                Operand::Register(1),
                Operand::Register(0),
                Operand::Imm32(0),
            ],
        ),
        (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
        (
            Op::StoreContextSlot,
            vec![Operand::Register(3), Operand::Register(1), coord(0, 0)],
        ),
        (
            Op::CreateContext,
            vec![
                Operand::Register(2),
                Operand::Register(1),
                Operand::Imm32(1),
            ],
        ),
    ];
    code.extend(body);
    let main = function(
        0,
        8,
        vec![
            plain(ScopeKind::Body, &[("v", SlotKind::Var)]),
            eval_var_scope(&[]),
        ],
        code,
    );
    module(vec![main], vec![string("v"), string("g")])
}

fn lookup_slot_v(dst: u16) -> (Op, Vec<Operand>) {
    (
        Op::LoadLookupSlot,
        vec![
            Operand::Register(dst),
            Operand::Register(2),
            Operand::ConstIndex(0),
            coord(1, 0),
        ],
    )
}

#[test]
fn an_eval_extension_shadows_an_outer_slot_until_deleted() {
    let program = lookup_module(vec![
        // No extension yet: the lookup reads the outer slot.
        lookup_slot_v(4),
        // `var v` from a sloppy eval in the inner scope, then `v = 9`.
        (
            Op::DeclareEvalVar,
            vec![
                Operand::Register(2),
                Operand::ConstIndex(0),
                Operand::Imm32(0),
            ],
        ),
        (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(9)]),
        (
            Op::StoreVarScope,
            vec![
                Operand::Register(3),
                Operand::Register(2),
                Operand::ConstIndex(0),
                Operand::Imm32(0),
            ],
        ),
        lookup_slot_v(5),
        // `delete v` removes the extension entry; the slot shows through.
        (
            Op::DeleteLookupSlot,
            vec![
                Operand::Register(6),
                Operand::Register(2),
                Operand::ConstIndex(0),
                Operand::Imm32(1),
            ],
        ),
        lookup_slot_v(7),
        // r4 * 100 + r5 * 10 + r7, with r6 == true checked by the sum below.
        (
            Op::LoadInt32,
            vec![Operand::Register(3), Operand::Imm32(100)],
        ),
        (
            Op::Mul,
            vec![
                Operand::Register(4),
                Operand::Register(4),
                Operand::Register(3),
            ],
        ),
        (
            Op::LoadInt32,
            vec![Operand::Register(3), Operand::Imm32(10)],
        ),
        (
            Op::Mul,
            vec![
                Operand::Register(5),
                Operand::Register(5),
                Operand::Register(3),
            ],
        ),
        (
            Op::Add,
            vec![
                Operand::Register(4),
                Operand::Register(4),
                Operand::Register(5),
            ],
        ),
        (
            Op::Add,
            vec![
                Operand::Register(4),
                Operand::Register(4),
                Operand::Register(7),
            ],
        ),
        (Op::Return, vec![Operand::Register(4)]),
    ]);
    assert_eq!(int(run(program).expect("lookups run")), 191);
}

#[test]
fn a_reference_resolved_before_the_rhs_survives_a_delete_during_it() {
    let store_ref = |value: u16, strict: bool| {
        (
            Op::StoreRef,
            vec![
                Operand::Register(value),
                Operand::Register(6),
                Operand::ConstIndex(0),
                Operand::Imm32(
                    StoreRefMode {
                        slot: Some(0),
                        fallback: BindingStoreFallback::Mutable,
                        strict,
                    }
                    .to_imm32(),
                ),
            ],
        )
    };
    let resolve = (
        Op::ResolveLookupRef,
        vec![
            Operand::Register(6),
            Operand::Register(2),
            Operand::ConstIndex(0),
            Operand::Imm32(LookupRefTarget::Slot(ContextCoord::new(1, 0).unwrap()).to_imm32()),
        ],
    );
    let delete = (
        Op::DeleteLookupSlot,
        vec![
            Operand::Register(5),
            Operand::Register(2),
            Operand::ConstIndex(0),
            Operand::Imm32(1),
        ],
    );
    // Sloppy: the reference names the extension; the RHS deletes the entry;
    // PutValue re-creates it (§9.1.1.1.5) and leaves the outer slot alone.
    let program = lookup_module(vec![
        (
            Op::DeclareEvalVar,
            vec![
                Operand::Register(2),
                Operand::ConstIndex(0),
                Operand::Imm32(0),
            ],
        ),
        resolve.clone(),
        delete.clone(),
        (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(7)]),
        store_ref(3, false),
        lookup_slot_v(4),
        (
            Op::LoadContextSlot,
            vec![Operand::Register(5), Operand::Register(1), coord(0, 0)],
        ),
        (
            Op::LoadInt32,
            vec![Operand::Register(3), Operand::Imm32(10)],
        ),
        (
            Op::Mul,
            vec![
                Operand::Register(4),
                Operand::Register(4),
                Operand::Register(3),
            ],
        ),
        (
            Op::Add,
            vec![
                Operand::Register(4),
                Operand::Register(4),
                Operand::Register(5),
            ],
        ),
        (Op::Return, vec![Operand::Register(4)]),
    ]);
    assert_eq!(int(run(program).expect("sloppy store through ref")), 71);

    // Strict: the same deleted reference is a ReferenceError.
    let program = lookup_module(vec![
        (
            Op::DeclareEvalVar,
            vec![
                Operand::Register(2),
                Operand::ConstIndex(0),
                Operand::Imm32(0),
            ],
        ),
        resolve.clone(),
        delete,
        (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(7)]),
        store_ref(3, true),
        (Op::ReturnUndefined, Vec::new()),
    ]);
    let error = run(program).expect_err("strict store through a deleted binding");
    assert!(error_text(&error).contains('v'), "{}", error_text(&error));

    // Without an extension entry the reference is the slot's context.
    let program = lookup_module(vec![
        resolve,
        (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(3)]),
        store_ref(3, false),
        (
            Op::LoadContextSlot,
            vec![Operand::Register(4), Operand::Register(1), coord(0, 0)],
        ),
        (Op::Return, vec![Operand::Register(4)]),
    ]);
    assert_eq!(int(run(program).expect("slot store through ref")), 3);
}

fn global_lookup_program(tail: Vec<(Op, Vec<Operand>)>) -> BytecodeModule {
    let global_mode = LookupGlobalMode {
        depth: 2,
        strict: false,
    }
    .to_imm32();
    let mut body = vec![
        (
            Op::LoadInt32,
            vec![Operand::Register(3), Operand::Imm32(11)],
        ),
        // `g = 11` with no binding anywhere: a sloppy global store.
        (
            Op::StoreLookupGlobal,
            vec![
                Operand::Register(3),
                Operand::Register(2),
                Operand::ConstIndex(1),
                Operand::Imm32(global_mode),
            ],
        ),
        // An eval `var g` in the inner scope now shadows the global.
        (
            Op::DeclareEvalVar,
            vec![
                Operand::Register(2),
                Operand::ConstIndex(1),
                Operand::Imm32(0),
            ],
        ),
    ];
    body.extend(tail);
    lookup_module(body)
}

fn global_lookup(op: Op, dst: u16) -> (Op, Vec<Operand>) {
    (
        op,
        vec![
            Operand::Register(dst),
            Operand::Register(2),
            Operand::ConstIndex(1),
            Operand::Imm32(2),
        ],
    )
}

#[test]
fn a_global_lookup_probes_extensions_before_the_global_record() {
    // The shadowing extension entry (still `undefined`) wins over the global.
    let shadowed = global_lookup_program(vec![
        global_lookup(Op::TypeofLookupGlobal, 4),
        (Op::Return, vec![Operand::Register(4)]),
    ]);
    assert!(run(shadowed).expect("shadowed lookup").is_undefined());

    // Deleting removes the extension entry first, so the global shows
    // through again; a second delete removes the global property.
    let unshadowed = global_lookup_program(vec![
        global_lookup(Op::DeleteLookupGlobal, 5),
        global_lookup(Op::LoadLookupGlobal, 6),
        (Op::Return, vec![Operand::Register(6)]),
    ]);
    assert_eq!(int(run(unshadowed).expect("global lookup")), 11);

    let deleted = global_lookup_program(vec![
        global_lookup(Op::DeleteLookupGlobal, 5),
        global_lookup(Op::DeleteLookupGlobal, 5),
        global_lookup(Op::TypeofLookupGlobal, 6),
        (Op::Return, vec![Operand::Register(6)]),
    ]);
    assert!(run(deleted).expect("typeof after delete").is_undefined());
}

#[test]
fn the_eval_caller_chain_lists_descriptors_extensions_and_the_var_scope() {
    let scopes = vec![
        eval_var_scope(&[("a", SlotKind::Var)]),
        plain(ScopeKind::Block, &[("b", SlotKind::Let)]),
    ];
    let main = function(
        0,
        1,
        scopes.clone(),
        vec![(Op::ReturnUndefined, Vec::new())],
    );
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interp
        .link_module(
            module(vec![main], vec![string("e")]),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verified fixture");
    let function_id = context.exec_main().id;
    let outer = interp
        .create_context_value(&context, function_id, 0, Value::undefined())
        .expect("outer context");
    let mut inner = interp
        .create_context_value(&context, function_id, 1, outer)
        .expect("inner context");
    let mut root = interp.persistent_root_insert(inner);
    interp
        .declare_eval_var_value(&context, function_id, inner, 0, 1)
        .expect("eval var");
    inner = interp.persistent_root_remove(root).expect("rooted inner");
    root = interp.persistent_root_insert(inner);

    let chain = interp
        .eval_caller_chain(&context, inner)
        .expect("caller chain");
    assert_eq!(chain.scopes.len(), 2);
    assert_eq!(chain.scopes[0].descriptor, scopes[1]);
    assert!(chain.scopes[0].extension_names.is_empty());
    assert_eq!(chain.scopes[1].descriptor, scopes[0]);
    assert_eq!(chain.scopes[1].extension_names, vec!["e".to_string()]);
    assert_eq!(chain.var_depth, Some(1));

    let empty = interp
        .eval_caller_chain(&context, Value::undefined())
        .expect("no context");
    assert!(empty.scopes.is_empty());
    assert_eq!(empty.var_depth, None);
    interp.persistent_root_remove(root);
}
