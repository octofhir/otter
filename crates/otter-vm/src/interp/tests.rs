// Split out of `lib.rs` `mod tests`.
#![allow(unused_imports)]
use crate::*;
use otter_bytecode::{
    Constant, Function, Instruction, Op, Operand, SourceKind as BcSourceKind, SpanEntry,
};

fn spans_for(code: &[Instruction]) -> Vec<SpanEntry> {
    code.iter()
        .map(|i| SpanEntry {
            pc: i.pc,
            span: (0, 0),
        })
        .collect()
}

fn test_function(
    id: u32,
    name: &str,
    param_count: u16,
    scratch: u16,
    code: Vec<Instruction>,
) -> Function {
    let spans = spans_for(&code);
    Function {
        id,
        name: name.to_string(),
        span: (0, 0),
        locals: 0,
        scratch,
        param_count,
        length: param_count,
        own_upvalue_count: 0,
        is_strict: false,
        is_arrow: false,
        is_method: false,
        has_rest: false,
        is_async: false,
        is_generator: false,
        is_async_generator: false,
        is_derived_constructor: false,
        is_module: false,
        needs_arguments: false,
        uses_arguments_callee: false,
        arguments_object_kind: ArgumentsObjectKind::Unmapped,
        mapped_argument_bindings: Vec::new(),
        source_text: None,
        source_text_span: None,
        module_url: String::new(),
        direct_eval_bindings: Vec::new(),
        contains_direct_eval: false,
        code: code.into(),
        spans,
    }
}

fn module_with(code: Vec<Instruction>, scratch: u16) -> BytecodeModule {
    BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, scratch, code)],
        constants: vec![],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}

fn jit_instr(op: Op, byte_pc: u32, operands: Vec<Operand>) -> jit::JitTestInstruction {
    jit::JitTestInstruction::new(op, byte_pc, byte_pc, 1, operands)
}

#[test]
fn osr_bail_policy_only_disables_target_loop_misses() {
    let instructions = vec![
        jit_instr(
            Op::LoadInt32,
            0,
            vec![Operand::Register(0), Operand::Imm32(0)],
        ),
        jit_instr(
            Op::Jump,
            40,
            vec![Operand::Imm32(-31)], // target = 40 + 1 - 31 = 10.
        ),
    ];

    let snapshot = jit::JitCompileSnapshot::without_feedback(0, 0, 4, instructions);
    assert!(Interpreter::osr_bail_inside_target_loop_instructions(
        &snapshot.code_block,
        &snapshot.instructions,
        10,
        30
    ));
    assert!(!Interpreter::osr_bail_inside_target_loop_instructions(
        &snapshot.code_block,
        &snapshot.instructions,
        10,
        50
    ));
}

#[test]
fn returns_undefined_for_load_then_return() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    assert_eq!(interp.run(&context).unwrap(), Value::undefined());
}

#[test]
fn strict_store_global_binding_rejects_non_writable_global_property() {
    let code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(0), Operand::Imm32(12)],
        },
        Instruction {
            pc: 1,
            op: Op::StoreGlobalBinding,
            operands: vec![
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Imm32(1),
            ],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 1, code)],
        constants: vec![Constant::String {
            utf16: "NaN".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);

    let err = interp
        .run(&context)
        .expect_err("strict assignment to NaN should throw");
    assert!(matches!(err.error, VmError::Uncaught));
    assert!(err.message().contains("TypeError"));
}

#[test]
fn load_string_constant_reuses_traced_cache_entry() {
    let code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadString,
            operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadString,
            operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(1)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, code)],
        constants: vec![Constant::String {
            utf16: "cached literal".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);

    assert!(interp.run(&context).unwrap().is_string());
    assert_eq!(interp.string_constant_cache_len_for_test(), 1);

    interp.force_gc().expect("force GC");

    assert!(interp.run(&context).unwrap().is_string());
    assert_eq!(interp.string_constant_cache_len_for_test(), 1);
}

#[test]
fn load_string_constant_cache_distinguishes_standalone_contexts() {
    fn module_with_string(name: &str, literal: &str) -> BytecodeModule {
        BytecodeModule {
            module: name.to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(
                0,
                "<main>",
                0,
                1,
                vec![
                    Instruction {
                        pc: 0,
                        op: Op::LoadString,
                        operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
                    },
                    Instruction {
                        pc: 1,
                        op: Op::Return,
                        operands: vec![Operand::Register(0)],
                    },
                ],
            )],
            constants: vec![Constant::String {
                utf16: literal.encode_utf16().collect(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    let mut interp = Interpreter::new();
    let first = ExecutionContext::from_module(module_with_string("first.ts", "first literal"));
    let second = ExecutionContext::from_module(module_with_string("second.ts", "second literal"));

    assert!(interp.run(&first).unwrap().is_string());
    assert!(interp.run(&second).unwrap().is_string());
    assert_eq!(interp.string_constant_cache_len_for_test(), 2);
}

#[test]
fn load_bigint_constant_reuses_traced_cache_entry() {
    let code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadBigInt,
            operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadBigInt,
            operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(1)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, code)],
        constants: vec![Constant::BigInt {
            decimal: "9007199254740993".to_string(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);

    assert!(interp.run(&context).unwrap().is_big_int());
    assert_eq!(interp.bigint_constant_cache_len_for_test(), 1);

    interp.force_gc().expect("force GC");

    assert!(interp.run(&context).unwrap().is_big_int());
    assert_eq!(interp.bigint_constant_cache_len_for_test(), 1);
}

#[test]
fn load_bigint_constant_cache_distinguishes_standalone_contexts() {
    fn module_with_bigint(name: &str, decimal: &str) -> BytecodeModule {
        BytecodeModule {
            module: name.to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![test_function(
                0,
                "<main>",
                0,
                1,
                vec![
                    Instruction {
                        pc: 0,
                        op: Op::LoadBigInt,
                        operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
                    },
                    Instruction {
                        pc: 1,
                        op: Op::Return,
                        operands: vec![Operand::Register(0)],
                    },
                ],
            )],
            constants: vec![Constant::BigInt {
                decimal: decimal.to_string(),
            }],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    let mut interp = Interpreter::new();
    let first = ExecutionContext::from_module(module_with_bigint("first.ts", "1"));
    let second = ExecutionContext::from_module(module_with_bigint("second.ts", "2"));

    assert!(interp.run(&first).unwrap().is_big_int());
    assert!(interp.run(&second).unwrap().is_big_int());
    assert_eq!(interp.bigint_constant_cache_len_for_test(), 2);
}

#[test]
fn direct_bytecode_call_binds_arguments_from_register_window() {
    let callee = test_function(
        1,
        "callee",
        3,
        2,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(3), Operand::Imm32(100)],
            },
            Instruction {
                pc: 1,
                op: Op::Mul,
                operands: vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(3),
                ],
            },
            Instruction {
                pc: 2,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(4), Operand::Imm32(10)],
            },
            Instruction {
                pc: 3,
                op: Op::Mul,
                operands: vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(4),
                ],
            },
            Instruction {
                pc: 4,
                op: Op::Add,
                operands: vec![
                    Operand::Register(3),
                    Operand::Register(3),
                    Operand::Register(4),
                ],
            },
            Instruction {
                pc: 5,
                op: Op::Add,
                operands: vec![
                    Operand::Register(3),
                    Operand::Register(3),
                    Operand::Register(2),
                ],
            },
            Instruction {
                pc: 6,
                op: Op::Return,
                operands: vec![Operand::Register(3)],
            },
        ],
    );
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(1)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(2), Operand::Imm32(2)],
        },
        Instruction {
            pc: 2,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(3), Operand::Imm32(3)],
        },
        Instruction {
            pc: 3,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(4), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 4,
            op: Op::Call,
            operands: vec![
                Operand::Register(0),
                Operand::Register(4),
                Operand::ConstIndex(3),
                Operand::Register(3),
                Operand::Register(1),
                Operand::Register(2),
            ],
        },
        Instruction {
            pc: 5,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 5, main_code), callee],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    assert_eq!(
        interp.run(&context).unwrap(),
        Value::number(NumberValue::Smi(312))
    );
}

#[test]
fn direct_bytecode_call_window_populates_arguments_object() {
    let mut callee = test_function(
        1,
        "callee",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::CollectArguments,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    callee.needs_arguments = true;
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(21)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(2), Operand::Imm32(34)],
        },
        Instruction {
            pc: 2,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(3), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 3,
            op: Op::Call,
            operands: vec![
                Operand::Register(0),
                Operand::Register(3),
                Operand::ConstIndex(2),
                Operand::Register(2),
                Operand::Register(1),
            ],
        },
        Instruction {
            pc: 4,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 4, main_code), callee],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    let Some(args) = (interp.run(&context).unwrap()).as_object() else {
        panic!("expected arguments object");
    };
    assert_eq!(
        object::get(args, interp.gc_heap(), "0"),
        Some(Value::number(NumberValue::Smi(34)))
    );
    assert_eq!(
        object::get(args, interp.gc_heap(), "1"),
        Some(Value::number(NumberValue::Smi(21)))
    );
    assert_eq!(
        object::get(args, interp.gc_heap(), "length"),
        Some(Value::number(NumberValue::Smi(2)))
    );
}

#[test]
fn direct_bytecode_call_window_populates_rest_arguments() {
    let mut callee = test_function(
        1,
        "callee",
        1,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::CollectRest,
                operands: vec![Operand::Register(1)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(1)],
            },
        ],
    );
    callee.has_rest = true;
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(5)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(2), Operand::Imm32(8)],
        },
        Instruction {
            pc: 2,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(3), Operand::Imm32(13)],
        },
        Instruction {
            pc: 3,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(4), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 4,
            op: Op::Call,
            operands: vec![
                Operand::Register(0),
                Operand::Register(4),
                Operand::ConstIndex(3),
                Operand::Register(1),
                Operand::Register(3),
                Operand::Register(2),
            ],
        },
        Instruction {
            pc: 5,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 5, main_code), callee],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let Some(rest) = (interp.run(&context).unwrap()).as_array() else {
        panic!("expected rest array");
    };
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    let elements = array::with_elements(rest, interp.gc_heap(), |elements| elements.to_vec());
    assert_eq!(
        elements,
        vec![
            Value::number(NumberValue::Smi(13)),
            Value::number(NumberValue::Smi(8))
        ]
    );
    assert!(
        after > before,
        "CollectRest should allocate the rest array in young space"
    );
}

#[test]
fn bytecode_store_property_function_bag_uses_young_allocation_with_frame_roots() {
    let callee = test_function(1, "callee", 0, 0, Vec::new());
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(42)],
        },
        Instruction {
            pc: 2,
            op: Op::StoreProperty,
            operands: vec![
                Operand::Register(0),
                Operand::ConstIndex(1),
                Operand::Register(1),
                Operand::Register(2),
            ],
        },
        Instruction {
            pc: 3,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, main_code), callee],
        constants: vec![
            Constant::FunctionId { index: 1 },
            Constant::String {
                utf16: "custom".encode_utf16().collect(),
            },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    let completion = interp.run(&context).unwrap();
    assert!(crate::is_callable(&completion));
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "StoreProperty should allocate function user props in young space"
    );
    let owner = completion
        .as_closure(interp.gc_heap())
        .expect("MakeFunction result is a per-instance closure");
    let desc = interp
        .ordinary_function_own_property_descriptor(Some(&context), Some(owner), 1, "custom")
        .unwrap()
        .expect("custom property descriptor");
    assert_eq!(
        descriptor_value(&desc),
        Value::number(NumberValue::from_i32(42))
    );
}

#[test]
fn bytecode_function_prototype_uses_young_allocation_with_frame_roots() {
    let module = module_with(Vec::new(), 4);
    let context = ExecutionContext::from_module(module.clone());
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::function(1);
    stack.push(frame);

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let prototype = interp
        .function_property_get_stack_rooted(&context, &stack, None, 1, "prototype")
        .expect("prototype");
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "Function .prototype should allocate user bag and prototype object in young space"
    );

    let Some(proto) = (prototype).as_object() else {
        panic!("function prototype should be an object");
    };
    assert_eq!(
        object::get(proto, interp.gc_heap(), "constructor"),
        Some(Value::function(1))
    );
}

#[test]
fn runtime_function_prototype_uses_young_allocation_with_explicit_roots() {
    let module = module_with(Vec::new(), 4);
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let target = Value::function(1);
    let arg = Value::string(JsString::from_str("rooted-arg", interp.gc_heap_mut()).unwrap());
    let args = [arg];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let prototype = interp
        .function_property_get_runtime_rooted(&context, None, 1, "prototype", &[&target], &[&args])
        .expect("prototype");
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "Function .prototype should allocate through runtime roots when no VM frame is active"
    );

    let Some(proto) = (prototype).as_object() else {
        panic!("function prototype should be an object");
    };
    assert_eq!(
        object::get(proto, interp.gc_heap(), "constructor"),
        Some(target)
    );
}

#[test]
fn bytecode_instanceof_function_prototype_uses_stack_roots() {
    let module = module_with(Vec::new(), 4);
    let context = ExecutionContext::from_module(module.clone());
    let mut interp = Interpreter::new();
    let lhs = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("lhs");
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[1] = Value::object(lhs);
    frame.registers[2] = Value::function(1);
    stack.push(frame);
    let operands = vec![
        Operand::Register(0),
        Operand::Register(1),
        Operand::Register(2),
    ];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        interp
            .drive_instanceof(&mut stack, &context, operands.as_slice())
            .expect("instanceof")
    );
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "instanceof should lazily allocate function .prototype through stack roots"
    );
    assert_eq!(stack[0].registers[0], Value::boolean(false));
    let desc = interp
        .ordinary_function_own_property_descriptor(Some(&context), None, 1, "prototype")
        .unwrap()
        .expect("prototype descriptor");
    assert!(descriptor_value(&desc).is_object());
}

#[test]
fn new_function_links_eval_chunk_into_shared_code_space() {
    let compiled_main = vec![
        Instruction {
            pc: 0,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let inner = test_function(1, "anonymous", 0, 1, Vec::new());
    let compiled = BytecodeModule {
        module: "eval.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 1, compiled_main), inner],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let outer = module_with(Vec::new(), 4);
    let mut interp = Interpreter::new();
    let context = interp.link_module(outer);
    interp.set_eval_hook(Some(std::sync::Arc::new(move |_, _| Ok(compiled.clone()))));
    let arg = Value::string(JsString::from_str("", interp.gc_heap_mut()).unwrap());
    let args = [arg];

    let result = interp
        .build_dynamic_function(
            &context,
            args.as_slice(),
            crate::eval_ops::DynamicFunctionKind::Normal,
        )
        .expect("Function constructor");

    let fid = result
        .as_function()
        .or_else(|| {
            result
                .as_closure(interp.gc_heap())
                .map(|c| c.cached_function_id)
        })
        .expect("function value");
    assert_eq!(
        fid, 2,
        "eval chunk ids rebase past the outer chunk's single function"
    );
    let function = context
        .function(fid)
        .expect("foreign id resolves through the shared code space");
    assert_eq!(function.name, "anonymous");
}

#[test]
fn get_iterator_map_snapshot_uses_old_iterator_state_allocation_with_frame_roots() {
    let module = module_with(Vec::new(), 5);
    let mut interp = Interpreter::new();
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        Value::number(NumberValue::from_i32(1)),
        Value::number(NumberValue::from_i32(10)),
    )
    .unwrap();
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        Value::number(NumberValue::from_i32(2)),
        Value::number(NumberValue::from_i32(20)),
    )
    .unwrap();

    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::map(map);
    stack.push(frame);

    let before = interp.gc_heap_mut().stats().old_allocated_bytes;
    interp.run_get_iterator_regs(&mut stack, 0, 1, 0).unwrap();
    let after = interp.gc_heap_mut().stats().old_allocated_bytes;
    assert!(
        after > before,
        "GetIterator over Map should allocate its iterator state in non-moving old space"
    );

    interp
        .run_iterator_next_regs(&mut stack[0], 2, 3, 1)
        .unwrap();
    assert_eq!(stack[0].registers[3], Value::boolean(false));
    let Some(pair) = (stack[0].registers[2]).as_array() else {
        panic!("Map iterator should yield entry arrays");
    };
    let values = crate::array::with_elements(pair, interp.gc_heap(), |elements| elements.to_vec());
    assert_eq!(
        values,
        vec![
            Value::number(NumberValue::from_i32(1)),
            Value::number(NumberValue::from_i32(10)),
        ]
    );
}

#[test]
fn get_iterator_user_resume_uses_old_iterator_state_allocation_with_frame_roots() {
    let module = module_with(Vec::new(), 4);
    let mut interp = Interpreter::new();
    let iterator_obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();

    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.pc = 0;
    interp.frame_ensure_cold(&mut frame).pending_get_iterator =
        Some(PendingGetIterator { pc: 0, dst: 1 });
    frame.registers[1] = Value::object(iterator_obj);
    stack.push(frame);
    let context = ExecutionContext::from_module(module);
    let operands = vec![Operand::Register(1), Operand::Register(0)];

    let before = interp.gc_heap_mut().stats().old_allocated_bytes;
    assert!(
        interp
            .drive_get_iterator(&mut stack, &context, operands.as_slice())
            .unwrap()
    );
    let after = interp.gc_heap_mut().stats().old_allocated_bytes;

    assert!(
        after > before,
        "GetIterator resume should allocate user iterator state in non-moving old space"
    );
    assert!(stack[0].registers[1].is_iterator());
    assert!(
        interp
            .frame_cold(&stack[0])
            .is_none_or(|c| c.pending_get_iterator.is_none())
    );
    assert_eq!(stack[0].pc, 1);
}

#[test]
fn array_callback_map_uses_stack_rooted_result_allocation() {
    fn identity_mapper(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        Ok(args.first().cloned().unwrap_or(Value::undefined()))
    }

    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
        constants: vec![Constant::String {
            utf16: "map".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let source = crate::array::from_elements_old_for_fixture(
        interp.gc_heap_mut(),
        [Value::number(NumberValue::from_i32(12))],
    )
    .unwrap();
    let mapper =
        native_value_static(interp.gc_heap_mut(), "identityMapper", 1, identity_mapper).unwrap();
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::array(source);
    frame.registers[1] = mapper;
    stack.push(frame);
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(2),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(1),
                Operand::Register(1),
            ],
        )
        .expect("array map");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "Array.prototype.map should allocate its result through stack roots"
    );
    let Some(result) = (stack[0].registers[2]).as_array() else {
        panic!("map should return an array");
    };
    assert_eq!(
        crate::array::get(result, interp.gc_heap(), 0),
        Value::number(NumberValue::from_i32(12))
    );
}

#[test]
fn call_method_on_nullish_receiver_reports_type_error() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "foo".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::undefined();
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("nullish method call should reject before intrinsic fallback");

    assert!(matches!(err, VmError::TypeError));
    assert_eq!(
        interp.error_detail(),
        Some(run_control::ErrorDetail::Message(
            "Cannot read properties of undefined".into()
        ))
    );
}

#[test]
fn call_method_on_missing_primitive_method_reports_not_callable() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "missing".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::number_i32(1);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("missing primitive method should reject as non-callable");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_string_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "slice".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("String")
        .expect("String.prototype")
        .as_object()
        .expect("String.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "slice",
        Value::number_i32(1),
    );
    let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = recv;
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable String.prototype.slice should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_string_char_code_at_builtin_fast_path() {
    let mut interp = Interpreter::new();
    let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());

    let stack = call_char_code_at(&mut interp, recv).expect("charCodeAt should resolve");

    assert_eq!(stack[0].registers[2], Value::number_i32(98));
}

#[test]
fn call_method_string_char_code_at_callable_shadow_falls_back() {
    fn replacement(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(777))
    }

    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("String")
        .expect("String.prototype")
        .as_object()
        .expect("String.prototype object");
    let replacement = native_value_static(interp.gc_heap_mut(), "replacement", 1, replacement)
        .expect("replacement");
    object::set(&mut proto, interp.gc_heap_mut(), "charCodeAt", replacement);
    let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());

    let stack = call_char_code_at(&mut interp, recv).expect("charCodeAt shadow should resolve");

    assert_eq!(stack[0].registers[2], Value::number_i32(777));
}

#[test]
fn call_method_string_char_code_at_non_callable_shadows_builtin() {
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("String")
        .expect("String.prototype")
        .as_object()
        .expect("String.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "charCodeAt",
        Value::number_i32(1),
    );
    let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());

    let err = call_char_code_at(&mut interp, recv)
        .expect_err("non-callable String.prototype.charCodeAt should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

fn call_char_code_at(interp: &mut Interpreter, recv: Value) -> Result<HoltStack, VmError> {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
        constants: vec![Constant::String {
            utf16: "charCodeAt".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = recv;
    frame.registers[1] = Value::number_i32(1);
    stack.push(frame);
    interp.do_call_method_value(
        &mut stack,
        &context,
        &[
            Operand::Register(2),
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::ConstIndex(1),
            Operand::Register(1),
        ],
    )?;
    Ok(stack)
}

#[test]
fn call_method_number_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "toString".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Number")
        .expect("Number.prototype")
        .as_object()
        .expect("Number.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "toString",
        Value::number_i32(1),
    );

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::number_i32(7);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Number.prototype.toString should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_number_to_string_builtin_fast_path() {
    let mut interp = Interpreter::new();

    let stack = call_number_to_string(&mut interp, Value::number_i32(42), None)
        .expect("Number.prototype.toString should resolve");

    let result = stack[0].registers[2]
        .as_string(interp.gc_heap_mut())
        .expect("string result")
        .to_lossy_string(interp.gc_heap());
    assert_eq!(result, "42");
    assert!(interp.small_int_string_cache[42].is_some());
}

#[test]
fn call_method_number_to_string_callable_shadow_falls_back() {
    fn replacement(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(777))
    }

    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Number")
        .expect("Number.prototype")
        .as_object()
        .expect("Number.prototype object");
    let replacement = native_value_static(interp.gc_heap_mut(), "replacement", 1, replacement)
        .expect("replacement");
    object::set(&mut proto, interp.gc_heap_mut(), "toString", replacement);

    let stack = call_number_to_string(&mut interp, Value::number_i32(42), None)
        .expect("Number.prototype.toString shadow should resolve");

    assert_eq!(stack[0].registers[2], Value::number_i32(777));
}

#[test]
fn call_method_number_to_string_non_decimal_radix_falls_back() {
    let mut interp = Interpreter::new();

    let stack = call_number_to_string(
        &mut interp,
        Value::number_i32(42),
        Some(Value::number_i32(16)),
    )
    .expect("Number.prototype.toString radix should resolve");

    let result = stack[0].registers[2]
        .as_string(interp.gc_heap_mut())
        .expect("string result")
        .to_lossy_string(interp.gc_heap());
    assert_eq!(result, "2a");
}

fn call_number_to_string(
    interp: &mut Interpreter,
    recv: Value,
    arg: Option<Value>,
) -> Result<HoltStack, VmError> {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 4, Vec::new())],
        constants: vec![Constant::String {
            utf16: "toString".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = recv;
    let has_arg = arg.is_some();
    if let Some(arg) = arg {
        frame.registers[1] = arg;
    }
    stack.push(frame);
    let operands = if !has_arg {
        vec![
            Operand::Register(2),
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::ConstIndex(0),
        ]
    } else {
        vec![
            Operand::Register(2),
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::ConstIndex(1),
            Operand::Register(1),
        ]
    };
    interp.do_call_method_value(&mut stack, &context, &operands)?;
    Ok(stack)
}

#[test]
fn call_method_boolean_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "valueOf".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Boolean")
        .expect("Boolean.prototype")
        .as_object()
        .expect("Boolean.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "valueOf",
        Value::number_i32(1),
    );

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::boolean(true);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Boolean.prototype.valueOf should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_bigint_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "toString".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("BigInt")
        .expect("BigInt.prototype")
        .as_object()
        .expect("BigInt.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "toString",
        Value::number_i32(1),
    );
    let bigint =
        crate::bigint::BigIntValue::from_i32(interp.gc_heap_mut(), 7).expect("bigint allocation");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::big_int(bigint);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable BigInt.prototype.toString should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_symbol_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "valueOf".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Symbol")
        .expect("Symbol.prototype")
        .as_object()
        .expect("Symbol.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "valueOf",
        Value::number_i32(1),
    );
    let symbol = JsSymbol::new(interp.gc_heap_mut(), None).expect("symbol allocation");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::symbol(symbol);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Symbol.prototype.valueOf should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_weak_ref_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "deref".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("WeakRef")
        .expect("WeakRef.prototype")
        .as_object()
        .expect("WeakRef.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "deref",
        Value::number_i32(1),
    );
    let target = Value::object(
        crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("target object"),
    );
    let weak_ref =
        crate::test_support::alloc_weak_ref(interp.gc_heap_mut(), &target).expect("weak ref");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::weak_ref(weak_ref);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable WeakRef.prototype.deref should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_finalization_registry_prototype_non_callable_shadows_builtin() {
    fn cleanup(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "unregister".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("FinalizationRegistry")
        .expect("FinalizationRegistry.prototype")
        .as_object()
        .expect("FinalizationRegistry.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "unregister",
        Value::number_i32(1),
    );
    let cleanup =
        native_value_static(interp.gc_heap_mut(), "cleanup", 0, cleanup).expect("cleanup function");
    let registry = crate::test_support::alloc_finalization_registry(interp.gc_heap_mut(), cleanup)
        .expect("registry");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::finalization_registry(registry);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable FinalizationRegistry.prototype.unregister should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_promise_expando_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "then".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let promise = promise_dispatch::pending_runtime_rooted(&mut interp, &[], &[]).unwrap();
    let mut bag = property_dispatch::promise_ensure_expando_pub(interp.gc_heap_mut(), &promise)
        .expect("promise expando");
    object::set(&mut bag, interp.gc_heap_mut(), "then", Value::number_i32(1));

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::promise(promise);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own promise method should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_promise_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "then".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let promise = promise_dispatch::pending_runtime_rooted(&mut interp, &[], &[]).unwrap();
    let mut proto = interp
        .constructor_prototype_value("Promise")
        .expect("Promise.prototype")
        .as_object()
        .expect("Promise.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "then",
        Value::number_i32(1),
    );

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::promise(promise);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Promise.prototype.then should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_array_own_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "map".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let array =
        crate::array::from_elements_old_for_fixture(interp.gc_heap_mut(), [Value::number_i32(1)])
            .expect("array allocation");
    crate::array::set_named_property(array, interp.gc_heap_mut(), "map", Value::number_i32(1))
        .expect("array expando property");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::array(array);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own array method should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_regexp_own_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "exec".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let units: Vec<u16> = "x".encode_utf16().collect();
    let regexp = JsRegExp::compile(interp.gc_heap_mut(), &units, "").expect("regexp");
    let mut bag = property_dispatch::regexp_ensure_expando_pub(interp.gc_heap_mut(), &regexp)
        .expect("regexp expando");
    object::set(&mut bag, interp.gc_heap_mut(), "exec", Value::number_i32(1));

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::regexp(regexp);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own regexp method should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_regexp_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "exec".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("RegExp")
        .expect("RegExp.prototype")
        .as_object()
        .expect("RegExp.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "exec",
        Value::number_i32(1),
    );
    let units: Vec<u16> = "x".encode_utf16().collect();
    let regexp = JsRegExp::compile(interp.gc_heap_mut(), &units, "").expect("regexp");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::regexp(regexp);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable RegExp.prototype.exec should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_date_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "getTime".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Date")
        .expect("Date.prototype")
        .as_object()
        .expect("Date.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "getTime",
        Value::number_i32(1),
    );
    let date =
        crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("date object");
    object::set_prototype(date, interp.gc_heap_mut(), Some(proto));
    object::set_date_data(date, interp.gc_heap_mut(), 0.0);

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::object(date);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Date.prototype.getTime should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_date_setter_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "setTime".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Date")
        .expect("Date.prototype")
        .as_object()
        .expect("Date.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "setTime",
        Value::number_i32(1),
    );
    let date =
        crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("date object");
    object::set_prototype(date, interp.gc_heap_mut(), Some(proto));
    object::set_date_data(date, interp.gc_heap_mut(), 0.0);

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::object(date);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Date.prototype.setTime should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_typed_array_own_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "map".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let buffer = crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
        .expect("array buffer");
    let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
    let typed_array = crate::binary::JsTypedArray::new(
        interp.gc_heap_mut(),
        buffer,
        crate::binary::TypedArrayKind::Int8,
        0,
        1,
    )
    .expect("typed array");
    let mut bag =
        property_dispatch::typed_array_ensure_expando_pub(interp.gc_heap_mut(), &typed_array)
            .expect("typed array expando");
    object::set(&mut bag, interp.gc_heap_mut(), "map", Value::number_i32(1));

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::typed_array(typed_array);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own typed array method should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_typed_array_callback_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "map".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Int8Array")
        .expect("Int8Array.prototype")
        .as_object()
        .expect("Int8Array.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "map",
        Value::number_i32(1),
    );
    let buffer = crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
        .expect("array buffer");
    let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
    let typed_array = crate::binary::JsTypedArray::new(
        interp.gc_heap_mut(),
        buffer,
        crate::binary::TypedArrayKind::Int8,
        0,
        1,
    )
    .expect("typed array");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::typed_array(typed_array);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Int8Array.prototype.map should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_typed_array_slice_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "slice".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Int8Array")
        .expect("Int8Array.prototype")
        .as_object()
        .expect("Int8Array.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "slice",
        Value::number_i32(1),
    );
    let buffer = crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
        .expect("array buffer");
    let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
    let typed_array = crate::binary::JsTypedArray::new(
        interp.gc_heap_mut(),
        buffer,
        crate::binary::TypedArrayKind::Int8,
        0,
        1,
    )
    .expect("typed array");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::typed_array(typed_array);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Int8Array.prototype.slice should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_iterator_prototype_non_callable_shadows_helper() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
        constants: vec![Constant::String {
            utf16: "toArray".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Iterator")
        .expect("Iterator.prototype")
        .as_object()
        .expect("Iterator.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "toArray",
        Value::number_i32(1),
    );
    let source = crate::array::from_elements_old_for_fixture(
        interp.gc_heap_mut(),
        [Value::number(NumberValue::from_i32(1))],
    )
    .expect("source array");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::array(source);
    stack.push(frame);
    interp
        .run_get_iterator_regs(&mut stack, 0, 1, 0)
        .expect("array iterator");
    stack[0].registers[0] = stack[0].registers[1];

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(2),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Iterator.prototype.toArray should shadow helper");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_map_prototype_non_callable_shadows_builtin_for_each() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "forEach".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Map")
        .expect("Map.prototype")
        .as_object()
        .expect("Map.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "forEach",
        Value::number_i32(1),
    );
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("map");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::map(map);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Map.prototype.forEach should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_set_prototype_non_callable_shadows_builtin_for_each() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "forEach".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Set")
        .expect("Set.prototype")
        .as_object()
        .expect("Set.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "forEach",
        Value::number_i32(1),
    );
    let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::set(set);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Set.prototype.forEach should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_map_prototype_non_callable_shadows_map_method() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "get".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Map")
        .expect("Map.prototype")
        .as_object()
        .expect("Map.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "get",
        Value::number_i32(1),
    );
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).expect("map");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::map(map);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Map.prototype.get should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_set_prototype_non_callable_shadows_set_add() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "add".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Set")
        .expect("Set.prototype")
        .as_object()
        .expect("Set.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "add",
        Value::number_i32(1),
    );
    let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::set(set);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Set.prototype.add should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_weak_map_prototype_non_callable_shadows_weak_map_method() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "get".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("WeakMap")
        .expect("WeakMap.prototype")
        .as_object()
        .expect("WeakMap.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "get",
        Value::number_i32(1),
    );
    let weak_map = crate::collections::alloc_weak_map(interp.gc_heap_mut()).expect("weak map");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::weak_map(weak_map);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable WeakMap.prototype.get should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_weak_set_prototype_non_callable_shadows_weak_set_method() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "add".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("WeakSet")
        .expect("WeakSet.prototype")
        .as_object()
        .expect("WeakSet.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "add",
        Value::number_i32(1),
    );
    let weak_set = crate::collections::alloc_weak_set(interp.gc_heap_mut()).expect("weak set");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::weak_set(weak_set);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable WeakSet.prototype.add should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_array_buffer_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "slice".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("ArrayBuffer")
        .expect("ArrayBuffer.prototype")
        .as_object()
        .expect("ArrayBuffer.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "slice",
        Value::number_i32(1),
    );
    let buffer = crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
        .expect("array buffer");
    let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::array_buffer(buffer);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable ArrayBuffer.prototype.slice should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_data_view_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "getUint8".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("DataView")
        .expect("DataView.prototype")
        .as_object()
        .expect("DataView.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "getUint8",
        Value::number_i32(1),
    );
    let buffer = crate::binary::alloc_local_array_buffer(interp.gc_heap_mut(), vec![0], None, None)
        .expect("array buffer");
    let buffer = crate::binary::JsArrayBuffer::from_local_handle(buffer);
    let view =
        crate::binary::JsDataView::new(interp.gc_heap_mut(), buffer, 0, 1).expect("data view");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::data_view(view);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable DataView.prototype.getUint8 should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_set_prototype_non_callable_shadows_es_set_method() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "union".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Set")
        .expect("Set.prototype")
        .as_object()
        .expect("Set.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "union",
        Value::number_i32(1),
    );
    let set = crate::collections::alloc_set(interp.gc_heap_mut()).expect("set");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::set(set);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Set.prototype.union should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_function_own_non_callable_shadows_call() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "target", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "call".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::function(0);
    stack.push(frame);
    let function_value = Value::function(0);
    let mut bag = interp
        .function_user_bag_stack_rooted(&stack, None, 0, &[&function_value])
        .expect("function user bag");
    object::set(&mut bag, interp.gc_heap_mut(), "call", Value::number_i32(1));

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own function call should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_function_own_non_callable_shadows_object_method() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "target", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "hasOwnProperty".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::function(0);
    stack.push(frame);
    let function_value = Value::function(0);
    let mut bag = interp
        .function_user_bag_stack_rooted(&stack, None, 0, &[&function_value])
        .expect("function user bag");
    object::set(
        &mut bag,
        interp.gc_heap_mut(),
        "hasOwnProperty",
        Value::number_i32(1),
    );

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable own hasOwnProperty should shadow Object.prototype");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_null_proto_object_missing_object_method_is_not_callable() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "hasOwnProperty".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let obj = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("object");
    object::set_prototype(obj, interp.gc_heap_mut(), None);

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::object(obj);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("null-prototype object should not inherit Object.prototype methods");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_native_function_object_prototype_non_callable_shadows_builtin() {
    fn noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "hasOwnProperty".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Object")
        .expect("Object.prototype")
        .as_object()
        .expect("Object.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "hasOwnProperty",
        Value::number_i32(1),
    );
    let native = native_value_static(interp.gc_heap_mut(), "target", 0, noop).expect("native");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = native;
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err("non-callable Object.prototype.hasOwnProperty should shadow native intercept");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_primitive_object_prototype_non_callable_shadows_builtin() {
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, Vec::new())],
        constants: vec![Constant::String {
            utf16: "hasOwnProperty".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let mut proto = interp
        .constructor_prototype_value("Object")
        .expect("Object.prototype")
        .as_object()
        .expect("Object.prototype object");
    object::set(
        &mut proto,
        interp.gc_heap_mut(),
        "hasOwnProperty",
        Value::number_i32(1),
    );

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::number_i32(1);
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(1),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect_err(
            "non-callable Object.prototype.hasOwnProperty should shadow primitive intercept",
        );

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn call_method_string_wrapper_replace_own_non_callable_shadows_builtin() {
    fn replacement(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 4, Vec::new())],
        constants: vec![Constant::String {
            utf16: "replace".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let proto = interp
        .constructor_prototype_value("String")
        .expect("String.prototype")
        .as_object()
        .expect("String.prototype object");
    let mut obj =
        object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("string wrapper");
    object::set_prototype(obj, interp.gc_heap_mut(), Some(proto));
    let data = JsString::from_str("abc", interp.gc_heap_mut()).expect("string data");
    object::set_string_data(obj, interp.gc_heap_mut(), data);
    object::set(
        &mut obj,
        interp.gc_heap_mut(),
        "replace",
        Value::number_i32(1),
    );
    let search = Value::string(JsString::from_str("a", interp.gc_heap_mut()).expect("search"));
    let repl =
        native_value_static(interp.gc_heap_mut(), "replacement", 1, replacement).expect("repl");

    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = Value::object(obj);
    frame.registers[1] = search;
    frame.registers[2] = repl;
    stack.push(frame);

    let err = interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(3),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(2),
                Operand::Register(1),
                Operand::Register(2),
            ],
        )
        .expect_err("non-callable own String wrapper replace should shadow builtin");

    assert!(matches!(err, VmError::NotCallable));
}

#[test]
fn array_symbol_iterator_factory_uses_native_rooted_iterator_allocation() {
    let module = module_with(Vec::new(), 2);
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let source = crate::array::from_elements_old_for_fixture(
        interp.gc_heap_mut(),
        [Value::number(NumberValue::from_i32(21))],
    )
    .unwrap();
    let factory = make_array_iterator_factory(source, interp.gc_heap_mut()).unwrap();
    let Some(native) = (factory).as_native_function() else {
        panic!("Array iterator factory should be native");
    };
    let call = native.call_target(interp.gc_heap());
    let before = interp.gc_heap_mut().stats().old_allocated_bytes;
    let call_info = NativeCallInfo::call(Value::undefined());
    let mut ctx = NativeCtx::new_with_call_info_and_context(&mut interp, call_info, Some(&context));

    let result = call.invoke(&mut ctx, &[]).expect("invoke iterator factory");

    let after = interp.gc_heap_mut().stats().old_allocated_bytes;
    assert!(
        after > before,
        "Array[Symbol.iterator] factory should allocate iterator state in non-moving old space"
    );
    let Some(iter) = (result).as_iterator() else {
        panic!("factory should return an iterator");
    };
    let (array, index) = interp.gc_heap().read_payload(iter, |state| match state {
        IteratorState::Array { array, index, .. } => (*array, *index),
        _ => panic!("factory should create an array iterator"),
    });
    assert_eq!(array, source);
    assert_eq!(index, 0);
}

#[test]
fn iterator_to_list_map_pairs_use_runtime_rooted_array_allocation() {
    let module = module_with(Vec::new(), 4);
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let map = crate::collections::alloc_map(interp.gc_heap_mut()).unwrap();
    crate::collections::map_set(
        map,
        interp.gc_heap_mut(),
        Value::number(NumberValue::from_i32(3)),
        Value::number(NumberValue::from_i32(30)),
    )
    .unwrap();
    let map_value = Value::map(map);
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    let entries = interp
        .iterator_to_list_sync(&context, &map_value)
        .expect("map entries");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "iterator_to_list_sync Map fast path should allocate pair arrays through runtime roots"
    );
    let Some(pair) = entries.first().and_then(|v| v.as_array()) else {
        panic!("expected pair array");
    };
    assert_eq!(
        crate::array::get(pair, interp.gc_heap(), 0),
        Value::number(NumberValue::from_i32(3))
    );
    assert_eq!(
        crate::array::get(pair, interp.gc_heap(), 1),
        Value::number(NumberValue::from_i32(30))
    );
}

#[test]
fn iterator_result_record_uses_runtime_rooted_young_allocation() {
    let mut interp = Interpreter::new();
    let value = Value::number(NumberValue::from_i32(44));
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    let result = interp
        .make_runtime_rooted_iter_result(value, true, &[], &[])
        .unwrap();

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "IteratorResult records should allocate through runtime roots"
    );
    let Some(record) = (result).as_object() else {
        panic!("IteratorResult should be an object");
    };
    assert_eq!(object::get(record, interp.gc_heap(), "value"), Some(value));
    assert_eq!(
        object::get(record, interp.gc_heap(), "done"),
        Some(Value::boolean(true))
    );
}

#[test]
fn new_collection_map_uses_root_aware_allocation_with_frame_roots() {
    let mut interp = Interpreter::new();
    let pair = crate::array::from_elements_old_for_fixture(
        interp.gc_heap_mut(),
        [
            Value::number(NumberValue::from_i32(1)),
            Value::number(NumberValue::from_i32(10)),
        ],
    )
    .unwrap();
    let seed =
        crate::array::from_elements_old_for_fixture(interp.gc_heap_mut(), [Value::array(pair)])
            .unwrap();
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, Vec::new())],
        constants: vec![Constant::String {
            utf16: "Map".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module.clone());
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[1] = Value::array(seed);
    stack.push(frame);

    let before_alloc = interp.gc_heap_mut().stats().new_allocated_bytes;
    let before_reserved = interp.gc_heap_mut().stats().reserved_bytes;
    interp
        .run_new_collection_regs(&context, &mut stack, 0, 0, 0, 1)
        .unwrap();
    let after_alloc = interp.gc_heap_mut().stats().new_allocated_bytes;
    let after_reserved = interp.gc_heap_mut().stats().reserved_bytes;

    assert!(
        after_alloc > before_alloc,
        "NewCollection Map should allocate the map body in young space"
    );
    assert!(
        after_reserved > before_reserved,
        "NewCollection Map should reserve backing storage through the root-aware path"
    );
    let Some(map) = (stack[0].registers[0]).as_map() else {
        panic!("NewCollection Map should write a Map");
    };
    assert_eq!(
        crate::collections::map_get(
            map,
            interp.gc_heap(),
            &Value::number(NumberValue::from_i32(1))
        ),
        Some(Value::number(NumberValue::from_i32(10)))
    );
}

#[test]
fn bytecode_new_error_uses_young_allocation_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(1)],
            },
            Instruction {
                pc: 1,
                op: Op::NewError,
                operands: vec![Operand::Register(0), Operand::Register(1)],
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        2,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    let Some(obj) = (interp.run(&context).unwrap()).as_object() else {
        panic!("NewError should return an object");
    };
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "NewError should allocate the error instance in young space"
    );
    assert!(crate::object::get_own_descriptor(obj, interp.gc_heap(), "message").is_none());
}

#[test]
fn vm_error_throwable_uses_stack_rooted_allocation() {
    let module = module_with(Vec::new(), 1);
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&module.functions[0]));
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    let error = interp
        .vm_error_to_throwable_with_stack_roots(None, &stack, &VmError::TypeMismatch)
        .and_then(|v| v.as_object())
        .expect("TypeMismatch should convert to a throwable object");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "VM error throwable conversion should allocate through stack roots"
    );
    let message_value = object::get(error, interp.gc_heap(), "message");
    let heap_ref = interp.gc_heap();
    let message = message_value
        .as_ref()
        .and_then(|v| v.as_string(heap_ref))
        .expect("message string");
    assert!(message.to_lossy_string(heap_ref).contains("type mismatch"));
}

#[test]
fn oom_throwable_uses_range_error_prototype() {
    let module = module_with(Vec::new(), 1);
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&module.functions[0]));

    let error = interp
        .vm_error_to_throwable_with_stack_roots(
            None,
            &stack,
            &VmError::OutOfMemory {
                requested_bytes: 160,
                heap_limit_bytes: 2 * 1024 * 1024,
            },
        )
        .and_then(|v| v.as_object())
        .expect("OutOfMemory should convert to a throwable object");

    assert!(object::has_in_proto_chain(
        error,
        interp.gc_heap(),
        interp.error_classes.prototype(ErrorKind::RangeError)
    ));
}

#[test]
fn host_rooted_object_and_array_helpers_use_young_allocation() {
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    let mut host = interp
        .alloc_host_object_with_roots(&[], &[])
        .expect("host object allocation");
    let host_root = Value::object(host);
    let elements = [Value::number(NumberValue::from_i32(1))];
    let array = interp
        .array_from_elements_host_rooted(
            elements.iter().cloned(),
            &[&host_root],
            &[elements.as_slice()],
        )
        .expect("host array allocation");
    object::set(
        &mut host,
        interp.gc_heap_mut(),
        "items",
        Value::array(array),
    );

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "host-rooted object and array helpers should allocate in young space"
    );
    assert!(object::get(host, interp.gc_heap(), "items").is_some_and(|v| v.is_array()));
}

#[test]
fn bytecode_new_weak_ref_uses_young_allocation_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::NewObject,
                operands: vec![Operand::Register(1)],
            },
            Instruction {
                pc: 1,
                op: Op::NewWeakRef,
                operands: vec![Operand::Register(0), Operand::Register(1)],
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        2,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_weak_ref());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "NewWeakRef should allocate the weak-ref body in young space"
    );
}

#[test]
fn bytecode_new_finalization_registry_uses_young_allocation_with_frame_roots() {
    let cleanup = test_function(1, "cleanup", 1, 1, Vec::new());
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::NewFinalizationRegistry,
            operands: vec![Operand::Register(0), Operand::Register(1)],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, main_code), cleanup],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_finalization_registry());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "NewFinalizationRegistry should allocate the registry body in young space"
    );
}

#[test]
fn direct_bytecode_async_call_window_populates_parameters() {
    let mut callee = test_function(
        1,
        "async_callee",
        1,
        1,
        vec![Instruction {
            pc: 0,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        }],
    );
    callee.is_async = true;
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(144)],
        },
        Instruction {
            pc: 1,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(2), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 2,
            op: Op::Call,
            operands: vec![
                Operand::Register(0),
                Operand::Register(2),
                Operand::ConstIndex(1),
                Operand::Register(1),
            ],
        },
        Instruction {
            pc: 3,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 3, main_code), callee],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    let Some(promise) = (interp.run(&context).unwrap()).as_promise() else {
        panic!("expected async function call to return a promise");
    };
    // Result promises live in old space (bodies must not move under
    // handle copies held across allocations); the state must survive
    // a full collection.
    interp.force_gc().expect("force GC");
    assert_eq!(
        promise.state(interp.gc_heap()),
        crate::promise::PromiseState::Fulfilled(Value::number(NumberValue::Smi(144)))
    );
}

#[test]
fn async_generator_method_uses_stack_rooted_capability_allocation() {
    let main = test_function(0, "<main>", 0, 1, Vec::new());
    let generator_body = test_function(
        1,
        "async_generator_body",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(0), Operand::Imm32(91)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![main.clone(), generator_body.clone()],
        constants: vec![Constant::String {
            utf16: "next".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let body_frame = Frame::for_function(&generator_body);
    let generator =
        crate::generator::JsGenerator::new(interp.gc_heap_mut(), body_frame).expect("gen");
    generator.set_async(interp.gc_heap_mut(), true);
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&main);
    frame.registers[0] = Value::generator(generator);
    stack.push(frame);

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    interp
        .do_call_method_value(
            &mut stack,
            &context,
            &[
                Operand::Register(0),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        )
        .expect("async generator next");
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    assert!(
        after > before,
        "async generator method should allocate its pending capability through stack roots"
    );
    assert!(stack[0].registers[0].is_promise());
}

#[test]
fn primitive_wrapper_boxing_uses_stack_rooted_young_allocation() {
    let main = test_function(0, "<main>", 0, 1, Vec::new());
    let callee = test_function(1, "sloppy_callee", 0, 1, Vec::new());
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![main.clone(), callee],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&main));
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    let boxed_this = interp
        .this_for_bytecode_call_stack_rooted(
            context.exec_function(1).expect("callee"),
            &stack,
            Value::number(NumberValue::from_i32(33)),
            &[],
        )
        .expect("boxed this");
    let primitive_string =
        Value::string(crate::JsString::from_str("abc", interp.gc_heap_mut()).unwrap());
    let property_base = interp
        .object_for_primitive_property_base_stack_rooted(&stack, &primitive_string)
        .expect("property base")
        .expect("primitive base");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "primitive wrapper boxing should allocate through stack-rooted young allocation"
    );
    assert!(boxed_this.is_object());
    assert!(Value::object(property_base).is_object());
}

#[test]
fn top_level_async_entry_returns_the_awaited_completion() {
    let mut main = test_function(
        0,
        "<main>",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(0), Operand::Imm32(512)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    main.is_async = true;
    main.is_module = true;
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![main],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    assert_eq!(
        interp.run(&context).unwrap(),
        Value::number(NumberValue::Smi(512))
    );
}

#[test]
fn promise_fulfilled_of_allocates_the_body_in_old_space() {
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(211)],
        },
        Instruction {
            pc: 1,
            op: Op::PromiseFulfilledOf,
            operands: vec![Operand::Register(0), Operand::Register(1)],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, main_code)],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    let Some(promise) = (interp.run(&context).unwrap()).as_promise() else {
        panic!("expected promise");
    };
    // Promise bodies allocate in old space: handle copies escape into
    // Rust locals across later allocations (combinators, reactions),
    // and a moving young body under a fixed copy was the
    // use-after-move family. The body must survive a full scavenge
    // untouched.
    interp.force_gc().expect("force GC");
    assert_eq!(
        promise.state(interp.gc_heap()),
        crate::promise::PromiseState::Fulfilled(Value::number(NumberValue::Smi(211)))
    );
}

#[test]
fn await_non_promise_uses_stack_rooted_wrapper_allocation() {
    let mut function = test_function(0, "async_body", 0, 1, Vec::new());
    function.is_async = true;
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![function],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let result_promise = {
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        JsPromiseHandle::pending_with_roots(interp.gc_heap_mut(), &mut external_visit)
            .expect("result promise")
    };
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.async_state = Some(AsyncFrameState { result_promise });
    let mut stack: HoltStack = HoltStack::new();
    stack.push(frame);
    let context = ExecutionContext::from_module(module);
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    interp
        .do_await(
            &mut stack,
            &context,
            0,
            Value::number(NumberValue::Smi(307)),
        )
        .expect("await");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "Await of a non-promise should wrap through stack-rooted young allocation"
    );
    assert!(stack.is_empty(), "await should park the active frame");
}

#[test]
fn promise_new_uses_stack_rooted_capability_allocation() {
    fn executor(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = module_with(Vec::new(), 3);
    let context = ExecutionContext::from_module(module.clone());
    let mut interp = Interpreter::new();
    let executor_value =
        native_value_static(interp.gc_heap_mut(), "executor", 2, executor).expect("executor");
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[1] = executor_value;
    stack.push(frame);
    let operands = vec![
        Operand::Register(0),
        Operand::Register(1),
        Operand::Register(2),
    ];
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    interp
        .run_promise_new_operands(&context, &mut stack, operands.as_slice())
        .expect("PromiseNew");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "PromiseNew should allocate its promise/capability through stack-rooted young allocation"
    );
    assert!(stack[0].registers[0].is_promise());
}

#[test]
fn dynamic_import_rejection_uses_stack_rooted_promise_allocation() {
    let module = module_with(Vec::new(), 2);
    let context = ExecutionContext::from_module(module.clone());
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[1] = Value::number(NumberValue::Smi(12));
    stack.push(frame);
    let operands = vec![Operand::Register(0), Operand::Register(1)];
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;

    interp
        .run_import_namespace_dynamic_operands(&context, &mut stack, 0, operands.as_slice())
        .expect("dynamic import");

    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "dynamic import rejection should allocate the TypeError and promise body through stack roots"
    );
    let Some(promise) = (stack[0].registers[0]).as_promise() else {
        panic!("expected promise");
    };
    let crate::promise::PromiseState::Rejected(reason_value) = promise.state(interp.gc_heap())
    else {
        panic!("expected TypeError rejection object");
    };
    let reason = reason_value
        .as_object()
        .expect("expected TypeError rejection object");
    let msg = object::get(reason, interp.gc_heap(), "message");
    let heap_ref = interp.gc_heap();
    let message = msg
        .as_ref()
        .and_then(|v| v.as_string(heap_ref))
        .expect("message string");
    // §13.3.10: the numeric specifier is coerced via ToString to
    // "12", then rejected because no module resolves under that
    // name (no loader is installed in this test).
    assert!(
        message
            .to_lossy_string(heap_ref)
            .contains("module not resolvable")
    );
}

#[test]
fn direct_bytecode_construct_window_populates_arguments_object() {
    let mut ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::CollectArguments,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    ctor.needs_arguments = true;
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(1), Operand::Imm32(55)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadInt32,
            operands: vec![Operand::Register(2), Operand::Imm32(89)],
        },
        Instruction {
            pc: 2,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(3), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 3,
            op: Op::New,
            operands: vec![
                Operand::Register(0),
                Operand::Register(3),
                Operand::ConstIndex(2),
                Operand::Register(2),
                Operand::Register(1),
            ],
        },
        Instruction {
            pc: 4,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    let Some(args) = (interp.run(&context).unwrap()).as_object() else {
        panic!("expected constructor-returned arguments object");
    };
    assert_eq!(
        object::get(args, interp.gc_heap(), "0"),
        Some(Value::number(NumberValue::Smi(89)))
    );
    assert_eq!(
        object::get(args, interp.gc_heap(), "1"),
        Some(Value::number(NumberValue::Smi(55)))
    );
}

#[test]
fn direct_bytecode_construct_receiver_uses_young_allocation_with_frame_roots() {
    let ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::New,
            operands: vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(0),
            ],
        },
        Instruction {
            pc: 2,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, main_code), ctor],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_object());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "ordinary bytecode constructor receiver should allocate in young space"
    );
}

#[test]
fn bound_bytecode_construct_receiver_uses_young_allocation_with_frame_roots() {
    let ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let main_code = vec![
        Instruction {
            pc: 0,
            op: Op::MakeFunction,
            operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
        },
        Instruction {
            pc: 1,
            op: Op::LoadUndefined,
            operands: vec![Operand::Register(2)],
        },
        Instruction {
            pc: 2,
            op: Op::BindFunction,
            operands: vec![
                Operand::Register(3),
                Operand::Register(1),
                Operand::Register(2),
                Operand::ConstIndex(0),
            ],
        },
        Instruction {
            pc: 3,
            op: Op::New,
            operands: vec![
                Operand::Register(0),
                Operand::Register(3),
                Operand::ConstIndex(0),
            ],
        },
        Instruction {
            pc: 4,
            op: Op::Return,
            operands: vec![Operand::Register(0)],
        },
    ];
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 4, main_code), ctor],
        constants: vec![Constant::FunctionId { index: 1 }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_object());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "bound bytecode constructor receiver should allocate in young space"
    );
}

#[test]
fn runtime_budget_stats_record_reductions_and_budget_observations() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    interp.set_runtime_budget(RuntimeBudget {
        max_reductions_per_turn: Some(1),
        ..RuntimeBudget::default()
    });
    let context = ExecutionContext::from_module(module);
    assert_eq!(interp.run(&context).unwrap(), Value::undefined());
    let stats = interp.runtime_budget_stats();
    assert_eq!(stats.turns_started, 1);
    assert_eq!(stats.turns_finished, 1);
    assert!(stats.reductions_executed >= 2);
    assert!(stats.max_turn_reductions >= 2);
    assert_eq!(stats.budget_limit_observations, 1);
    assert_eq!(stats.max_stack_depth_observed, 1);
}

#[test]
fn runtime_budget_can_reject_on_reduction_limit() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    interp.set_runtime_budget(RuntimeBudget {
        on_exceeded: RuntimeBudgetExceededAction::Reject,
        max_reductions_per_turn: Some(0),
        ..RuntimeBudget::default()
    });
    let context = ExecutionContext::from_module(module);
    let err = interp.run(&context).unwrap_err();
    assert!(matches!(err.error, VmError::BudgetExceeded));
    let stats = interp.runtime_budget_stats();
    assert_eq!(stats.budget_rejections, 1);
    assert_eq!(stats.budget_limit_observations, 1);
}

#[test]
fn runtime_budget_stats_record_heap_allocations() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::NewObject,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_object());
    let stats = interp.runtime_budget_stats();
    assert!(stats.allocated_objects_observed >= 1);
    assert!(stats.allocated_bytes_observed > 0);
    assert!(stats.max_turn_allocated_bytes > 0);
    assert!(stats.max_live_heap_bytes > 0);
}

#[test]
fn bytecode_new_object_uses_young_allocation_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::NewObject,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_object());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "NewObject should allocate the object body in young space"
    );
}

#[test]
fn bytecode_new_array_uses_young_allocation_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(0), Operand::Imm32(42)],
            },
            Instruction {
                pc: 1,
                op: Op::NewArray,
                operands: vec![
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(0),
                ],
            },
            Instruction {
                pc: 2,
                op: Op::Return,
                operands: vec![Operand::Register(1)],
            },
        ],
        2,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let context = ExecutionContext::from_module(module);
    assert!(interp.run(&context).unwrap().is_array());
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;
    assert!(
        after > before,
        "NewArray should allocate the array body in young space"
    );
}

#[test]
fn bytecode_array_push_uses_root_aware_growth_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(0), Operand::Imm32(1)],
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(2)],
            },
            Instruction {
                pc: 2,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(3)],
            },
            Instruction {
                pc: 3,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(3), Operand::Imm32(4)],
            },
            Instruction {
                pc: 4,
                op: Op::NewArray,
                operands: vec![
                    Operand::Register(4),
                    Operand::ConstIndex(4),
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            },
            Instruction {
                pc: 5,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(5), Operand::Imm32(5)],
            },
            Instruction {
                pc: 6,
                op: Op::ArrayPush,
                operands: vec![Operand::Register(4), Operand::Register(5)],
            },
            Instruction {
                pc: 7,
                op: Op::Return,
                operands: vec![Operand::Register(4)],
            },
        ],
        6,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().reserved_bytes;
    let context = ExecutionContext::from_module(module);
    let result = interp.run(&context).unwrap();
    let Some(array) = (result).as_array() else {
        panic!("ArrayPush program should return the grown array");
    };
    let values =
        crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
    assert_eq!(values.len(), 5);
    assert_eq!(values[4], Value::number(NumberValue::from_i32(5)));
    let after = interp.gc_heap_mut().stats().reserved_bytes;
    assert!(
        after > before,
        "ArrayPush should reserve dense backing storage through the root-aware path"
    );
}

#[test]
fn bytecode_store_element_uses_root_aware_growth_with_frame_roots() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(0), Operand::Imm32(1)],
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(1), Operand::Imm32(2)],
            },
            Instruction {
                pc: 2,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(2), Operand::Imm32(3)],
            },
            Instruction {
                pc: 3,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(3), Operand::Imm32(4)],
            },
            Instruction {
                pc: 4,
                op: Op::NewArray,
                operands: vec![
                    Operand::Register(4),
                    Operand::ConstIndex(4),
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            },
            Instruction {
                pc: 5,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(5), Operand::Imm32(4)],
            },
            Instruction {
                pc: 6,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(6), Operand::Imm32(99)],
            },
            Instruction {
                pc: 7,
                op: Op::StoreElement,
                operands: vec![
                    Operand::Register(4),
                    Operand::Register(5),
                    Operand::Register(6),
                    Operand::Register(7),
                ],
            },
            Instruction {
                pc: 8,
                op: Op::Return,
                operands: vec![Operand::Register(4)],
            },
        ],
        8,
    );
    let mut interp = Interpreter::new();
    let before = interp.gc_heap_mut().stats().reserved_bytes;
    let context = ExecutionContext::from_module(module);
    let result = interp.run(&context).unwrap();
    let Some(array) = (result).as_array() else {
        panic!("StoreElement program should return the grown array");
    };
    let values =
        crate::array::with_elements(array, interp.gc_heap_mut(), |elements| elements.to_vec());
    assert_eq!(values.len(), 5);
    assert_eq!(values[4], Value::number(NumberValue::from_i32(99)));
    let after = interp.gc_heap_mut().stats().reserved_bytes;
    assert!(
        after > before,
        "StoreElement should reserve dense backing storage through the root-aware path"
    );
}

#[test]
fn runtime_budget_stats_record_host_ops_and_external_bytes() {
    let mut interp = Interpreter::new();
    interp.set_runtime_budget(RuntimeBudget {
        max_host_ops_per_turn: Some(0),
        max_external_bytes: Some(0),
        ..RuntimeBudget::default()
    });

    interp.begin_runtime_budget_turn();
    interp.record_runtime_host_op_enqueued();
    let external = interp.gc_heap_mut().reserve_external(64).unwrap();
    interp.finish_runtime_budget_turn();

    let stats = interp.runtime_budget_stats();
    assert_eq!(stats.host_ops_enqueued, 1);
    assert_eq!(stats.max_turn_host_ops, 1);
    assert!(stats.max_external_bytes_observed >= 64);
    assert!(stats.budget_limit_observations >= 1);
    drop(external);
}

#[test]
fn missing_return_errors() {
    let module = module_with(
        vec![Instruction {
            pc: 0,
            op: Op::Nop,
            operands: vec![],
        }],
        0,
    );
    let mut interp = Interpreter::new();
    let context = ExecutionContext::from_module(module);
    assert_eq!(
        interp.run(&context).unwrap_err().error,
        VmError::MissingReturn
    );
}

#[test]
fn unwind_throw_pops_frames_until_handler_or_uncaught() {
    // No handlers anywhere in the stack: the throw escapes as
    // VmError::Uncaught carrying the rendered value.
    let main = Function {
        id: 0,
        name: "<main>".to_string(),
        span: (0, 0),
        locals: 0,
        scratch: 1,
        param_count: 0,
        length: 0,
        own_upvalue_count: 0,
        is_strict: false,
        is_arrow: false,
        is_method: false,
        has_rest: false,
        is_async: false,
        is_generator: false,
        is_async_generator: false,
        is_derived_constructor: false,
        is_module: false,
        needs_arguments: false,
        uses_arguments_callee: false,
        arguments_object_kind: ArgumentsObjectKind::Unmapped,
        mapped_argument_bindings: Vec::new(),
        source_text: None,
        source_text_span: None,
        module_url: String::new(),
        direct_eval_bindings: Vec::new(),
        contains_direct_eval: false,
        code: vec![Instruction {
            pc: 0,
            op: Op::ReturnUndefined,
            operands: vec![],
        }]
        .into(),
        spans: vec![SpanEntry {
            pc: 0,
            span: (0, 0),
        }],
    };
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&main));
    // Push a second frame on top — should be popped during
    // unwinding and not absorb the throw.
    stack.push(Frame::for_function(&main));
    let context = ExecutionContext::from_module(module_with(
        vec![Instruction {
            pc: 0,
            op: Op::ReturnUndefined,
            operands: vec![],
        }],
        1,
    ));
    let mut interp = Interpreter::new();
    let err = interp
        .unwind_throw(&context, &mut stack, Value::boolean(true))
        .unwrap_err();
    match err {
        VmError::Uncaught => assert_eq!(
            interp.error_detail(),
            Some(run_control::ErrorDetail::Uncaught("true".into()))
        ),
        other => panic!("expected Uncaught, got {other:?}"),
    }
    assert!(stack.is_empty(), "frames should be drained on uncaught");
}

#[test]
fn unwind_throw_lands_in_catch_handler() {
    let main = Function {
        id: 0,
        name: "<main>".to_string(),
        span: (0, 0),
        locals: 0,
        scratch: 2,
        param_count: 0,
        length: 0,
        own_upvalue_count: 0,
        is_strict: false,
        is_arrow: false,
        is_method: false,
        has_rest: false,
        is_async: false,
        is_generator: false,
        is_async_generator: false,
        is_derived_constructor: false,
        is_module: false,
        needs_arguments: false,
        uses_arguments_callee: false,
        arguments_object_kind: ArgumentsObjectKind::Unmapped,
        mapped_argument_bindings: Vec::new(),
        source_text: None,
        source_text_span: None,
        module_url: String::new(),
        direct_eval_bindings: Vec::new(),
        contains_direct_eval: false,
        code: vec![Instruction {
            pc: 0,
            op: Op::ReturnUndefined,
            operands: vec![],
        }]
        .into(),
        spans: vec![SpanEntry {
            pc: 0,
            span: (0, 0),
        }],
    };
    let mut interp = Interpreter::new();
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&main);
    interp
        .frame_ensure_cold(&mut frame)
        .handlers
        .push(TryHandler {
            catch_pc: Some(42),
            finally_pc: None,
            exc_register: 1,
        });
    stack.push(frame);
    let context = ExecutionContext::from_module(module_with(
        vec![Instruction {
            pc: 0,
            op: Op::ReturnUndefined,
            operands: vec![],
        }],
        2,
    ));
    interp
        .unwind_throw(&context, &mut stack, Value::boolean(true))
        .unwrap();
    assert_eq!(stack[0].pc, 42);
    assert_eq!(stack[0].registers[1], Value::boolean(true));
    assert!(
        interp
            .frame_cold(&stack[0])
            .is_none_or(|c| c.handlers.is_empty())
    );
}

#[test]
fn is_callable_recognises_call_shapes() {
    assert!(is_callable(&Value::function(7)));
    let mut closure_heap = otter_gc::GcHeap::new().expect("closure heap");
    let closure_handle =
        crate::closure::alloc_closure(&mut closure_heap, 7, Vec::new(), None, None, None, None)
            .expect("closure");
    assert!(is_callable(&Value::closure(closure_handle)));
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");
    let bound = BoundFunction::new(
        &mut heap,
        Value::function(7),
        Value::undefined(),
        SmallVec::new(),
    )
    .expect("bound");
    assert!(is_callable(&Value::bound_function(bound)));
    assert!(!is_callable(&Value::number(NumberValue::Smi(1))));
    assert!(!is_callable(&Value::object(
        crate::object::alloc_object_old_for_fixture(&mut heap).unwrap()
    )));
}

#[test]
fn native_call_context_receives_method_receiver() {
    fn return_this(ctx: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(*ctx.this_value())
    }

    let module = module_with(vec![], 1);
    let mut interp = Interpreter::new();
    let callee =
        native_value_static(interp.gc_heap_mut(), "returnThis", 0, return_this).expect("native");
    let receiver =
        Value::object(crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap());
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&module.functions[0]));
    let context = ExecutionContext::from_module(module.clone());

    interp
        .invoke(&mut stack, &context, &callee, receiver, SmallVec::new(), 0)
        .unwrap();

    assert_eq!(stack[0].registers[0], receiver);
}

#[test]
fn direct_native_call_uses_contiguous_argument_window() {
    fn sum_smi_args(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let mut sum = 0;
        for arg in args {
            match arg.as_number().and_then(|n| n.as_smi()) {
                Some(n) => sum += n,
                None => {
                    return Err(NativeError::TypeError {
                        name: "sum",
                        reason: "expected smi".to_string(),
                    });
                }
            }
        }
        Ok(Value::number(NumberValue::Smi(sum)))
    }

    let module = module_with(vec![], 4);
    let mut interp = Interpreter::new();
    let callee = native_value_static(interp.gc_heap_mut(), "sum", 2, sum_smi_args).expect("native");
    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = callee;
    frame.registers[1] = Value::number(NumberValue::Smi(8));
    frame.registers[2] = Value::number(NumberValue::Smi(13));
    stack.push(frame);
    let context = ExecutionContext::from_module(module.clone());
    let operands = vec![
        Operand::Register(3),
        Operand::Register(0),
        Operand::ConstIndex(2),
        Operand::Register(1),
        Operand::Register(2),
    ];

    interp.do_call(&mut stack, &context, &operands).unwrap();

    assert_eq!(stack[0].registers[3], Value::number(NumberValue::Smi(21)));
}

#[test]
fn proxy_call_argv_array_uses_young_allocation_with_frame_roots() {
    fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
    }

    fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = module_with(vec![], 4);
    let mut interp = Interpreter::new();
    let apply = native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
    let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
    let mut handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
    object::set(&mut handler, interp.gc_heap_mut(), "apply", apply);
    let proxy = Value::proxy(
        crate::proxy::JsProxy::new(interp.gc_heap_mut(), target, Value::object(handler)).unwrap(),
    );

    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[0] = proxy;
    frame.registers[1] = Value::number(NumberValue::Smi(7));
    frame.registers[2] = Value::number(NumberValue::Smi(11));
    stack.push(frame);
    let context = ExecutionContext::from_module(module.clone());
    let operands = vec![
        Operand::Register(3),
        Operand::Register(0),
        Operand::ConstIndex(2),
        Operand::Register(1),
        Operand::Register(2),
    ];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    interp.do_call(&mut stack, &context, &operands).unwrap();
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    let Some(argv) = (stack[0].registers[3]).as_array() else {
        panic!("expected proxy apply argv array");
    };
    let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
    assert_eq!(
        elements,
        vec![
            Value::number(NumberValue::Smi(7)),
            Value::number(NumberValue::Smi(11)),
        ]
    );
    assert!(
        after > before,
        "proxy apply argv array should allocate in young space"
    );
}

#[test]
fn proxy_construct_argv_array_uses_young_allocation_with_frame_roots() {
    fn return_proxy_arg(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
    }

    let ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 2, vec![]), ctor],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    let construct =
        native_value_static(interp.gc_heap_mut(), "construct", 3, return_proxy_arg).unwrap();
    let mut handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
    object::set(&mut handler, interp.gc_heap_mut(), "construct", construct);
    let proxy = Value::proxy(
        crate::proxy::JsProxy::new(
            interp.gc_heap_mut(),
            Value::function(1),
            Value::object(handler),
        )
        .unwrap(),
    );

    let mut stack: HoltStack = HoltStack::new();
    let mut frame = Frame::for_function(&module.functions[0]);
    frame.registers[1] = proxy;
    stack.push(frame);
    let context = ExecutionContext::from_module(module.clone());
    let operands = vec![
        Operand::Register(0),
        Operand::Register(1),
        Operand::ConstIndex(0),
    ];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    interp
        .do_construct(&mut stack, &context, &operands)
        .unwrap();
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    assert!(stack[0].registers[0].is_proxy());
    assert!(
        after > before,
        "proxy construct argv array should allocate in young space"
    );
}

#[test]
fn run_callable_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
    fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        Ok(args.get(2).cloned().unwrap_or(Value::undefined()))
    }

    fn target_noop(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
    }

    let module = module_with(Vec::new(), 1);
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let apply = native_value_static(interp.gc_heap_mut(), "apply", 3, return_argv_array).unwrap();
    let target = native_value_static(interp.gc_heap_mut(), "target", 0, target_noop).unwrap();
    let mut handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
    object::set(&mut handler, interp.gc_heap_mut(), "apply", apply);
    let proxy = Value::proxy(
        crate::proxy::JsProxy::new(interp.gc_heap_mut(), target, Value::object(handler)).unwrap(),
    );
    let args: SmallVec<[Value; 8]> = smallvec::smallvec![
        Value::number(NumberValue::Smi(3)),
        Value::number(NumberValue::Smi(5)),
    ];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let result = interp
        .run_callable_sync(&context, &proxy, Value::undefined(), args)
        .unwrap();
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    let Some(argv) = (result).as_array() else {
        panic!("proxy apply trap should return the synthesized argv array");
    };
    let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
    assert_eq!(
        elements,
        vec![
            Value::number(NumberValue::Smi(3)),
            Value::number(NumberValue::Smi(5)),
        ]
    );
    assert!(
        after > before,
        "run_callable_sync proxy argv array should allocate in young space"
    );
}

#[test]
fn run_construct_sync_receiver_uses_runtime_rooted_young_allocation() {
    let ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 1, Vec::new()), ctor],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let target = Value::function(1);

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let result = interp
        .run_construct_sync(&context, &target, target, SmallVec::new())
        .unwrap();
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    assert!(result.is_object());
    assert!(
        after > before,
        "run_construct_sync should allocate the receiver in young space"
    );
}

#[test]
fn run_construct_sync_proxy_argv_array_uses_runtime_rooted_young_allocation() {
    fn return_argv_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        Ok(args.get(1).cloned().unwrap_or(Value::undefined()))
    }

    let ctor = test_function(
        1,
        "Ctor",
        0,
        1,
        vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
    );
    let module = BytecodeModule {
        module: "test.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![test_function(0, "<main>", 0, 1, Vec::new()), ctor],
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let context = ExecutionContext::from_module(module);
    let mut interp = Interpreter::new();
    let construct =
        native_value_static(interp.gc_heap_mut(), "construct", 3, return_argv_array).unwrap();
    let mut handler = object::alloc_object_old_for_fixture(interp.gc_heap_mut()).unwrap();
    object::set(&mut handler, interp.gc_heap_mut(), "construct", construct);
    let proxy = Value::proxy(
        crate::proxy::JsProxy::new(
            interp.gc_heap_mut(),
            Value::function(1),
            Value::object(handler),
        )
        .unwrap(),
    );
    let args: SmallVec<[Value; 8]> = smallvec::smallvec![Value::number(NumberValue::Smi(13))];

    let before = interp.gc_heap_mut().stats().new_allocated_bytes;
    let result = interp
        .run_construct_sync(&context, &proxy, proxy, args)
        .unwrap();
    let after = interp.gc_heap_mut().stats().new_allocated_bytes;

    let Some(argv) = (result).as_array() else {
        panic!("proxy construct trap should return the synthesized argv array");
    };
    let elements = array::with_elements(argv, interp.gc_heap(), |elements| elements.to_vec());
    assert_eq!(elements, vec![Value::number(NumberValue::Smi(13))]);
    assert!(
        after > before,
        "run_construct_sync proxy argv array should allocate in young space"
    );
}

#[test]
fn arrow_closure_overrides_call_site_this() {
    // <main>: r0 = LoadThis; Return r0
    // The arrow closure wraps function id 1 with `is_arrow=true`
    // and a `bound_this = Some({tag: "outer"})`. We sneak the
    // bound `this` in by hand-building the closure value rather
    // than going through the full call sequence — the unit test
    // is proving that the arrow's lexical receiver wins, not
    // that the compiler emits the right opcode (the engine
    // suite's `arrow-this.ts` covers the latter).
    let main = Function {
        id: 0,
        name: "<main>".to_string(),
        span: (0, 0),
        locals: 0,
        scratch: 1,
        param_count: 0,
        length: 0,
        own_upvalue_count: 0,
        is_strict: false,
        is_arrow: false,
        is_method: false,
        has_rest: false,
        is_async: false,
        is_generator: false,
        is_async_generator: false,
        is_derived_constructor: false,
        is_module: false,
        needs_arguments: false,
        uses_arguments_callee: false,
        arguments_object_kind: ArgumentsObjectKind::Unmapped,
        mapped_argument_bindings: Vec::new(),
        source_text: None,
        source_text_span: None,
        module_url: String::new(),
        direct_eval_bindings: Vec::new(),
        contains_direct_eval: false,
        code: vec![Instruction {
            pc: 0,
            op: Op::ReturnUndefined,
            operands: vec![],
        }]
        .into(),
        spans: vec![SpanEntry {
            pc: 0,
            span: (0, 0),
        }],
    };
    let arrow = Function {
        id: 1,
        name: "<arrow>".to_string(),
        span: (0, 0),
        locals: 0,
        scratch: 1,
        param_count: 0,
        length: 0,
        own_upvalue_count: 0,
        is_strict: false,
        is_arrow: true,
        is_method: false,
        has_rest: false,
        is_async: false,
        is_generator: false,
        is_async_generator: false,
        is_derived_constructor: false,
        is_module: false,
        needs_arguments: false,
        uses_arguments_callee: false,
        arguments_object_kind: ArgumentsObjectKind::Unmapped,
        mapped_argument_bindings: Vec::new(),
        source_text: None,
        source_text_span: None,
        module_url: String::new(),
        direct_eval_bindings: Vec::new(),
        contains_direct_eval: false,
        code: vec![
            Instruction {
                pc: 0,
                op: Op::LoadThis,
                operands: vec![Operand::Register(0)],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnValue,
                operands: vec![Operand::Register(0)],
            },
        ]
        .into(),
        spans: vec![SpanEntry {
            pc: 0,
            span: (0, 0),
        }],
    };
    let module = BytecodeModule {
        module: "arrow.ts".to_string(),
        template_sites: Vec::new(),
        source_kind: BcSourceKind::TypeScript,
        functions: vec![main, arrow],
        constants: vec![],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    // Build the closure by hand and dispatch via `invoke`. The
    // bound_this is a marker string — if `LoadThis` returns it,
    // the lexical override is working.
    let mut interp = Interpreter::new();
    let bound = JsString::from_str("outer", interp.gc_heap_mut()).unwrap();
    let closure_handle = crate::closure::alloc_closure(
        interp.gc_heap_mut(),
        1,
        Vec::new(),
        Some(Value::string(bound)),
        None,
        None,
        None,
    )
    .expect("closure alloc");
    let closure = Value::closure(closure_handle);
    let mut stack: HoltStack = HoltStack::new();
    stack.push(Frame::for_function(&module.functions[0]));
    let context = ExecutionContext::from_module(module.clone());
    // Caller-supplied this is `Null` — the closure must override.
    interp
        .invoke(
            &mut stack,
            &context,
            &closure,
            Value::null(),
            SmallVec::new(),
            /* dst */ 0,
        )
        .unwrap();
    // Drive the arrow's body to completion, then read r0 of <main>.
    loop {
        let top = stack.len() - 1;
        let f = module
            .functions
            .get(stack[top].function_id as usize)
            .unwrap();
        let pc = stack[top].pc as usize;
        let instr = &f.code[pc];
        if matches!(instr.op, Op::ReturnValue) {
            let value = stack[top].registers[0];
            stack.pop();
            let caller = stack.last_mut().unwrap();
            let dst = caller.return_register.unwrap_or(0) as usize;
            caller.registers[dst] = value;
            break;
        }
        if matches!(instr.op, Op::LoadThis) {
            let dst = match f.code.operand(instr, 0).expect("LoadThis dst") {
                Operand::Register(r) => r,
                _ => unreachable!(),
            };
            let value = stack[top].this_value;
            stack[top].registers[dst as usize] = value;
            stack[top].pc += 1;
            continue;
        }
        unreachable!();
    }
    assert_eq!(stack[0].registers[0], Value::string(bound));
}

#[test]
fn interrupt_handle_breaks_loop() {
    let module = module_with(
        vec![
            Instruction {
                pc: 0,
                op: Op::Nop,
                operands: vec![],
            },
            Instruction {
                pc: 1,
                op: Op::Return,
                operands: vec![Operand::Register(0)],
            },
        ],
        1,
    );
    let mut interp = Interpreter::new();
    let handle = interp.interrupt_handle();
    handle.interrupt();
    let context = ExecutionContext::from_module(module);
    assert_eq!(
        interp.run(&context).unwrap_err().error,
        VmError::Interrupted
    );
}

#[test]
fn call_target_feedback_tracks_mono_then_poly() {
    let mut interp = Interpreter::new();
    // Two distinct call sites in caller fid 7.
    let site_a = 12u32;
    let site_b = 40u32;

    // Site A only ever sees callee 3 → stays Mono(3).
    interp.note_call_target(7, site_a, 3);
    interp.note_call_target(7, site_a, 3);
    interp.note_call_target(7, site_a, 3);
    assert_eq!(
        interp.jit_call_site_feedback.get(&(7, site_a)),
        Some(&CallTargetFeedback::Mono(3))
    );

    // Site B sees 3 then 9 → promotes to Poly and stays there.
    interp.note_call_target(7, site_b, 3);
    assert_eq!(
        interp.jit_call_site_feedback.get(&(7, site_b)),
        Some(&CallTargetFeedback::Mono(3))
    );
    interp.note_call_target(7, site_b, 9);
    assert_eq!(
        interp.jit_call_site_feedback.get(&(7, site_b)),
        Some(&CallTargetFeedback::Poly)
    );
    interp.note_call_target(7, site_b, 3);
    assert_eq!(
        interp.jit_call_site_feedback.get(&(7, site_b)),
        Some(&CallTargetFeedback::Poly)
    );

    // Same site PC under a different caller is independent.
    interp.note_call_target(99, site_a, 5);
    assert_eq!(
        interp.jit_call_site_feedback.get(&(99, site_a)),
        Some(&CallTargetFeedback::Mono(5))
    );
    assert_eq!(
        interp.jit_call_site_feedback.get(&(7, site_a)),
        Some(&CallTargetFeedback::Mono(3))
    );
}

#[test]
fn arith_feedback_accumulates_per_site_and_bakes_into_view() {
    let mut interp = Interpreter::new();

    // A pure-int32 site under fid 5 at byte-PC 16.
    interp.current_function_id = 5;
    interp.current_byte_pc = 16;
    interp.note_arith(Value::number_i32(1), Value::number_i32(2));
    interp.note_arith(Value::number_i32(7), Value::number_i32(-3));
    let int_site = interp.jit_arith_feedback.get(&(5, 16)).copied().unwrap();
    assert!(int_site.is_int32_only());
    assert!(int_site.is_numeric_only());

    // A second site that mixes int32 and double → numeric but not int32.
    interp.current_byte_pc = 32;
    interp.note_arith(Value::number_i32(1), Value::number_f64(2.5));
    let num_site = interp.jit_arith_feedback.get(&(5, 32)).copied().unwrap();
    assert!(!num_site.is_int32_only());
    assert!(num_site.is_numeric_only());

    // Same byte-PC under a different fid is independent (never recorded).
    assert!(!interp.jit_arith_feedback.contains_key(&(9, 16)));

    // Baking copies each site's bits into the matching instruction by
    // byte-PC; unobserved instructions stay 0.
    let mut view = jit::JitCompileSnapshot::without_feedback(
        5,
        0,
        4,
        vec![16u32, 32, 48]
            .into_iter()
            .map(|byte_pc| {
                jit::JitTestInstruction::new(
                    Op::Add,
                    byte_pc / 16,
                    byte_pc,
                    16,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                )
            })
            .collect(),
    );
    interp.bake_arith_feedback(&mut view, 5);
    assert_eq!(view.instructions[0].arith_feedback, int_site.bits());
    assert_eq!(view.instructions[1].arith_feedback, num_site.bits());
    assert_eq!(view.instructions[2].arith_feedback, 0);

    // A widened overflow site forces numeric mixed feedback even if the
    // interpreter never observed a double operand there.
    interp.jit_arith_widen_float.insert((5, 48));
    for instr in &mut view.instructions {
        instr.arith_feedback = 0;
    }
    interp.bake_arith_feedback(&mut view, 5);
    assert_eq!(view.instructions[0].arith_feedback, int_site.bits());
    assert_eq!(view.instructions[1].arith_feedback, num_site.bits());
    assert_eq!(
        view.instructions[2].arith_feedback,
        jit_feedback::ARITH_INT32 | jit_feedback::ARITH_FLOAT64
    );
}
