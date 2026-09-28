//! Tests for the context binding model: scope descriptors, context
//! creation, static depths, per-iteration copies, eval lookups, and the
//! derived-constructor `this` slot.
//!
//! # Contents
//! - [`assert_verified`] — bytecode verification plus a static context-chain
//!   check every compiler test runs.
//! - [`ChainChecker`] — reconstructs, from `MakeClosure` / `CreateContext` /
//!   `CopyContext` / `LoadClosureContext`, the descriptor chain each context
//!   operand names, and checks every coordinate against it.
//! - behavior tests for each rule of the model.
//!
//! # Invariants
//! - Context registers follow lexical scope nesting, so the most recent
//!   linear-order write of a context register before a use is the context
//!   that use sees.
//!
//! # See also
//! - `function_context` and `compiler` for the model under test.

use crate::*;
use otter_bytecode::{
    BytecodeModule, ContextCoord, EvalCallerChain, EvalCallerScope, LookupRefTarget,
    ScopeDescriptor, ScopeFlags, ScopeKind, SlotDescriptor, SlotKind, StoreRefMode,
};

/// Where a chain entry's descriptor lives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChainLink {
    /// `Function::scopes[scope]` of `function`.
    Local { function: usize, scope: usize },
    /// Entry `hop` of an eval caller chain.
    Caller { hop: usize },
}

/// Static context-chain reconstruction over one module.
pub(crate) struct ChainChecker<'m> {
    module: &'m BytecodeModule,
    caller: Option<&'m EvalCallerChain>,
    /// Closure chain of each function, from its creation site.
    closure_chains: Vec<Option<Vec<ChainLink>>>,
}

fn register(operand: Option<Operand>) -> Option<u16> {
    match operand? {
        Operand::Register(reg) => Some(reg),
        _ => None,
    }
}

fn imm(operand: Option<Operand>) -> Option<i32> {
    match operand? {
        Operand::Imm32(value) => Some(value),
        _ => None,
    }
}

fn constant(operand: Option<Operand>) -> Option<u32> {
    match operand? {
        Operand::ConstIndex(value) => Some(value),
        _ => None,
    }
}

impl<'m> ChainChecker<'m> {
    pub(crate) fn new(module: &'m BytecodeModule, caller: Option<&'m EvalCallerChain>) -> Self {
        let mut checker = Self {
            module,
            caller,
            closure_chains: vec![None; module.functions.len()],
        };
        // Function 0: a script / eval `<main>` closes over the caller chain
        // (empty for scripts); a module-init over its runtime-created scope 0.
        let main_chain = if module.functions.first().is_some_and(|f| f.is_module) {
            vec![ChainLink::Local {
                function: 0,
                scope: 0,
            }]
        } else {
            (0..caller.map_or(0, |c| c.scopes.len()))
                .map(|hop| ChainLink::Caller { hop })
                .collect()
        };
        checker.closure_chains[0] = Some(main_chain);
        // Creation sites always precede the created function's use, and a
        // parent function precedes its children in the table only loosely,
        // so iterate to a fixed point.
        loop {
            let mut changed = false;
            for function in 0..module.functions.len() {
                if checker.closure_chains[function].is_none() {
                    continue;
                }
                let code = &module.functions[function].code;
                for (pc, instruction) in code.iter().enumerate() {
                    let (target, chain) = match instruction.op {
                        Op::MakeClosure => {
                            let Some(target) = constant(code.operand(instruction, 1))
                                .and_then(|k| checker.function_constant(k))
                            else {
                                continue;
                            };
                            let ctx = register(code.operand(instruction, 2)).expect("ctx");
                            (target, checker.chain_at(function, pc, ctx))
                        }
                        Op::MakeFunction => {
                            let Some(target) = constant(code.operand(instruction, 1))
                                .and_then(|k| checker.function_constant(k))
                            else {
                                continue;
                            };
                            (target, Some(Vec::new()))
                        }
                        _ => continue,
                    };
                    if checker.closure_chains[target].is_none() && chain.is_some() {
                        checker.closure_chains[target] = chain;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        checker
    }

    fn function_constant(&self, index: u32) -> Option<usize> {
        match self.module.constants.get(index as usize)? {
            Constant::FunctionId { index } => Some(*index as usize),
            _ => None,
        }
    }

    fn string_constant(&self, index: u32) -> Option<String> {
        match self.module.constants.get(index as usize)? {
            Constant::String { utf16 } => String::from_utf16(utf16).ok(),
            _ => None,
        }
    }

    /// The chain the context in register `reg` heads at `pc` of `function`.
    /// `None` when the register holds no statically known context.
    fn chain_at(&self, function: usize, pc: usize, reg: u16) -> Option<Vec<ChainLink>> {
        let code = &self.module.functions[function].code;
        for back in (0..pc).rev() {
            let instruction = &code[back];
            let writes = match instruction.op {
                Op::CreateContext
                | Op::CopyContext
                | Op::LoadClosureContext
                | Op::LoadUndefined => register(code.operand(instruction, 0)) == Some(reg),
                _ => false,
            };
            if !writes {
                continue;
            }
            return match instruction.op {
                Op::LoadClosureContext => self.closure_chains[function].clone(),
                Op::LoadUndefined => Some(Vec::new()),
                Op::CopyContext => {
                    let src = register(code.operand(instruction, 1))?;
                    self.chain_at(function, back, src)
                }
                Op::CreateContext => {
                    let parent = register(code.operand(instruction, 1))?;
                    let scope = imm(code.operand(instruction, 2))? as usize;
                    let mut chain = vec![ChainLink::Local { function, scope }];
                    chain.extend(self.chain_at(function, back, parent)?);
                    Some(chain)
                }
                _ => unreachable!(),
            };
        }
        None
    }

    fn descriptor(&self, link: &ChainLink) -> &ScopeDescriptor {
        match link {
            ChainLink::Local { function, scope } => {
                &self.module.functions[*function].scopes[*scope]
            }
            ChainLink::Caller { hop } => {
                &self.caller.expect("caller chain").scopes[*hop].descriptor
            }
        }
    }

    /// The slot a context coordinate names, checked against its chain.
    pub(crate) fn slot_at(
        &self,
        function: usize,
        pc: usize,
        ctx: u16,
        coord: ContextCoord,
    ) -> Result<&SlotDescriptor, String> {
        let chain = self
            .chain_at(function, pc, ctx)
            .ok_or_else(|| format!("fn {function} pc {pc}: r{ctx} holds no known context"))?;
        let link = chain.get(usize::from(coord.depth)).ok_or_else(|| {
            format!(
                "fn {function} pc {pc}: depth {} beyond chain of {}",
                coord.depth,
                chain.len()
            )
        })?;
        let descriptor = self.descriptor(link);
        descriptor
            .slots
            .get(usize::from(coord.slot))
            .ok_or_else(|| {
                format!(
                    "fn {function} pc {pc}: slot {} beyond {:?} scope of {} slots",
                    coord.slot,
                    descriptor.kind,
                    descriptor.slots.len()
                )
            })
    }

    /// Check every context operand in the module.
    pub(crate) fn check(&self) -> Result<(), String> {
        for (function, record) in self.module.functions.iter().enumerate() {
            if self.closure_chains[function].is_none() {
                // Never created in this module (unreachable code).
                continue;
            }
            let code = &record.code;
            for (pc, instruction) in code.iter().enumerate() {
                let op = instruction.op;
                let operand = |index| code.operand(instruction, index);
                match op {
                    Op::CreateContext => {
                        let scope = imm(operand(2)).unwrap_or(-1);
                        if scope < 0 || scope as usize >= record.scopes.len() {
                            return Err(format!("fn {function} pc {pc}: bad scope {scope}"));
                        }
                        let parent = register(operand(1)).unwrap();
                        if self.chain_at(function, pc, parent).is_none() {
                            return Err(format!(
                                "fn {function} pc {pc}: parent r{parent} holds no context"
                            ));
                        }
                    }
                    Op::LoadContextSlot
                    | Op::LoadContextSlotChecked
                    | Op::StoreContextSlot
                    | Op::StoreContextSlotChecked
                    | Op::BindThisContextSlot => {
                        let ctx = register(operand(1)).unwrap();
                        let coord =
                            ContextCoord::from_imm32(imm(operand(2)).unwrap()).ok_or("coord")?;
                        let slot = self.slot_at(function, pc, ctx, coord)?;
                        if op == Op::BindThisContextSlot && slot.kind != SlotKind::DerivedThis {
                            return Err(format!("fn {function} pc {pc}: bind into {slot:?}"));
                        }
                    }
                    Op::LoadLookupSlot | Op::StoreLookupSlot => {
                        let ctx = register(operand(1)).unwrap();
                        let name = self.string_constant(constant(operand(2)).unwrap());
                        let coord =
                            ContextCoord::from_imm32(imm(operand(3)).unwrap()).ok_or("coord")?;
                        let slot = self.slot_at(function, pc, ctx, coord)?;
                        if Some(&slot.name) != name.as_ref() {
                            return Err(format!(
                                "fn {function} pc {pc}: lookup {name:?} lands on slot {:?}",
                                slot.name
                            ));
                        }
                    }
                    Op::ResolveLookupRef => {
                        let ctx = register(operand(1)).unwrap();
                        let name = self.string_constant(constant(operand(2)).unwrap());
                        if let LookupRefTarget::Slot(coord) =
                            LookupRefTarget::from_imm32(imm(operand(3)).unwrap())
                        {
                            let slot = self.slot_at(function, pc, ctx, coord)?;
                            if Some(&slot.name) != name.as_ref() {
                                return Err(format!(
                                    "fn {function} pc {pc}: ref {name:?} lands on {:?}",
                                    slot.name
                                ));
                            }
                        }
                    }
                    Op::LoadLookupGlobal
                    | Op::TypeofLookupGlobal
                    | Op::DeleteLookupGlobal
                    | Op::DeleteLookupSlot => {
                        let ctx = register(operand(1)).unwrap();
                        let depth = imm(operand(3)).unwrap() as usize;
                        let chain = self.chain_at(function, pc, ctx).ok_or("lookup ctx")?;
                        if depth > chain.len() {
                            return Err(format!("fn {function} pc {pc}: depth {depth} > chain"));
                        }
                    }
                    Op::MakeClosure | Op::Eval | Op::CopyContext => {
                        let index = match op {
                            Op::MakeClosure => 2,
                            Op::Eval => 2,
                            _ => 1,
                        };
                        let ctx = register(operand(index)).unwrap();
                        if self.chain_at(function, pc, ctx).is_none() {
                            return Err(format!(
                                "fn {function} pc {pc}: {op:?} context r{ctx} unknown"
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

/// Verify a compiled module structurally and check its context chains.
pub(crate) fn assert_verified(module: &BytecodeModule) {
    assert_verified_with_caller(module, None);
}

pub(crate) fn assert_verified_with_caller(
    module: &BytecodeModule,
    caller: Option<&EvalCallerChain>,
) {
    if let Err(error) = otter_bytecode::verify_module(module) {
        panic!(
            "bytecode verification failed: {error:?}\n{}",
            otter_bytecode::disasm::disassemble(module)
        );
    }
    if let Err(error) = ChainChecker::new(module, caller).check() {
        panic!(
            "context chain check failed: {error}\n{}",
            otter_bytecode::disasm::disassemble(module)
        );
    }
}

fn compile(src: &str) -> BytecodeModule {
    let module = compile_script_source(src, SyntaxSourceKind::JavaScript, "test.js")
        .unwrap_or_else(|error| panic!("compile failed: {error:?}\n{src}"));
    assert_verified(&module);
    module
}

fn function<'m>(module: &'m BytecodeModule, name: &str) -> &'m Function {
    module
        .functions
        .iter()
        .find(|function| function.name == name)
        .unwrap_or_else(|| panic!("function {name}"))
}

fn function_index(module: &BytecodeModule, name: &str) -> usize {
    module
        .functions
        .iter()
        .position(|function| function.name == name)
        .unwrap_or_else(|| panic!("function {name}"))
}

fn ops(function: &Function) -> Vec<Op> {
    function
        .code
        .iter()
        .map(|instruction| instruction.op)
        .collect()
}

/// Every `(depth, slot name)` a function's context-slot accesses of `op`
/// resolve to.
fn slot_accesses(module: &BytecodeModule, name: &str, op: Op) -> Vec<(u16, String)> {
    let checker = ChainChecker::new(module, None);
    let index = function_index(module, name);
    let code = &module.functions[index].code;
    code.iter()
        .enumerate()
        .filter(|(_, instruction)| instruction.op == op)
        .map(|(pc, instruction)| {
            let ctx = register(code.operand(instruction, 1)).unwrap();
            let coord = ContextCoord::from_imm32(imm(code.operand(instruction, 2)).unwrap())
                .expect("coord");
            let slot = checker
                .slot_at(index, pc, ctx, coord)
                .unwrap_or_else(|error| panic!("{error}"));
            (coord.depth, slot.name.clone())
        })
        .collect()
}

fn slot_names(scope: &ScopeDescriptor) -> Vec<&str> {
    scope.slots.iter().map(|slot| slot.name.as_str()).collect()
}

#[test]
fn slots_are_allocated_per_scope() {
    let module = compile(
        "function f() { let x = 1; { let x = 2; g = () => x; } h = () => x; return [g, h]; }",
    );
    let f = function(&module, "f");
    assert_eq!(f.scopes.len(), 2, "{:?}", f.scopes);
    assert_eq!(f.scopes[0].kind, ScopeKind::Body);
    assert_eq!(slot_names(&f.scopes[0]), ["x"]);
    assert_eq!(f.scopes[1].kind, ScopeKind::Block);
    assert_eq!(slot_names(&f.scopes[1]), ["x"]);
    assert_eq!(f.scopes[1].slots[0].kind, SlotKind::Let);
    // Each arrow reads the `x` of its own scope at depth 0; the chain
    // checker (run by `compile`) proves the block arrow's context is the
    // block's and the other's the body's.
    for arrow in ["g", "h"] {
        assert_eq!(
            slot_accesses(&module, arrow, Op::LoadContextSlotChecked),
            [(0, "x".to_string())]
        );
    }
    let checker = ChainChecker::new(&module, None);
    let heads: Vec<ChainLink> = ["g", "h"]
        .iter()
        .map(|arrow| {
            let index = function_index(&module, arrow);
            checker.closure_chains[index].as_ref().unwrap()[0].clone()
        })
        .collect();
    assert_ne!(heads[0], heads[1]);
}

#[test]
fn contexts_are_created_only_for_scopes_with_slots() {
    let module = compile("function f(a) { { let b = a; return () => b; } }");
    let f = function(&module, "f");
    // `a` is not captured: the body scope owns no context.
    assert_eq!(f.scopes.len(), 1);
    assert_eq!(f.scopes[0].kind, ScopeKind::Block);
    let code = ops(f);
    let create = code.iter().position(|op| *op == Op::CreateContext).unwrap();
    let make = code.iter().position(|op| *op == Op::MakeClosure).unwrap();
    assert!(create < make, "{code:?}");
    assert_eq!(
        code.iter().filter(|op| **op == Op::CreateContext).count(),
        1
    );
    // A script-level function's closure context is statically empty: the
    // block context's parent is `undefined` and the function needs no
    // context of its own.
    assert_eq!(code.first(), Some(&Op::LoadUndefined));
    assert!(!code.contains(&Op::LoadClosureContext));
    assert!(ops(module.main()).contains(&Op::MakeFunction));
    assert!(!ops(module.main()).contains(&Op::LoadClosureContext));
    // Nested one level down, the parent is the creator's context.
    let nested =
        compile("function o() { let z; return function f(a) { { let b = z; return () => b; } }; }");
    let f = function(&nested, "f");
    assert_eq!(ops(f).first(), Some(&Op::LoadClosureContext));

    let plain = compile("function g(a, b) { let c = a + b; return c; }");
    let g = function(&plain, "g");
    assert!(g.scopes.is_empty());
    assert!(!ops(g).contains(&Op::CreateContext));
    assert!(!ops(g).contains(&Op::LoadClosureContext));
    // A capture-free declaration materializes without a context.
    assert!(ops(plain.main()).contains(&Op::MakeFunction));
}

#[test]
fn depths_count_only_context_bearing_scopes_across_functions() {
    let module = compile(
        "function a() { let x = 1; return function b() { return function c() { \
         let q = 0; { let y = 2; return () => x + y + q; } }; }; }",
    );
    // `b` owns no context but passes its closure context on.
    let b = function(&module, "b");
    assert!(b.scopes.is_empty());
    let b_ops = ops(b);
    assert_eq!(b_ops.first(), Some(&Op::LoadClosureContext));
    assert!(b_ops.contains(&Op::MakeClosure));
    // The arrow sees: its block `y` (0), `c`'s body `q` (1), `a`'s `x` (2).
    let arrow = module
        .functions
        .iter()
        .find(|function| function.is_arrow)
        .unwrap()
        .name
        .clone();
    let mut reads = slot_accesses(&module, &arrow, Op::LoadContextSlotChecked);
    reads.sort();
    assert_eq!(
        reads,
        [
            (0, "y".to_string()),
            (1, "q".to_string()),
            (2, "x".to_string())
        ]
    );
}

#[test]
fn let_loop_heads_copy_their_context_per_iteration() {
    let module = compile(
        "function f(fs) { for (let i = 0, j = () => i; i < 3; i++) fs.push(() => i); \
         for (const k of [1]) fs.push(() => k); for (const c = 0; c < 1;) break; return j; }",
    );
    let f = function(&module, "f");
    let code = &f.code;
    let copies: Vec<(u16, u16)> = code
        .iter()
        .filter(|instruction| instruction.op == Op::CopyContext)
        .map(|instruction| {
            (
                register(code.operand(instruction, 0)).unwrap(),
                register(code.operand(instruction, 1)).unwrap(),
            )
        })
        .collect();
    // One copy after the initializer, one before the increment; each copies
    // the head context in place. The `const` heads never copy.
    assert_eq!(copies.len(), 2, "{:?}", ops(f));
    assert!(copies.iter().all(|(dst, src)| dst == src));
    assert_eq!(copies[0], copies[1]);
    let head = f
        .scopes
        .iter()
        .position(|scope| scope.kind == ScopeKind::ForHead && slot_names(scope) == ["i"]);
    assert!(head.is_some(), "{:?}", f.scopes);
    // `for (const k of …)` creates its per-iteration context after
    // IteratorNext, inside the loop.
    let ops = ops(f);
    let next = ops.iter().position(|op| *op == Op::IteratorNext).unwrap();
    assert!(
        ops[next..].contains(&Op::CreateContext),
        "per-iteration context after IteratorNext: {ops:?}"
    );
}

#[test]
fn parameter_expressions_get_separate_scopes() {
    let module = compile("function f(a = () => b, b = 2) { var c = a; return () => c; }");
    let f = function(&module, "f");
    assert_eq!(f.scopes[0].kind, ScopeKind::Params);
    assert_eq!(slot_names(&f.scopes[0]), ["b"]);
    assert_eq!(f.scopes[0].slots[0].kind, SlotKind::Param { checked: true });
    assert!(!f.scopes[0].flags.var_scope);
    let body = &f.scopes[1];
    assert_eq!(body.kind, ScopeKind::Body);
    assert!(body.flags.var_scope);
    assert_eq!(slot_names(body), ["c"]);
    // The default-parameter closure reads the later parameter through the
    // checked (TDZ) path.
    let default = module
        .functions
        .iter()
        .find(|function| function.is_arrow)
        .unwrap()
        .name
        .clone();
    assert_eq!(
        slot_accesses(&module, &default, Op::LoadContextSlotChecked),
        [(0, "b".to_string())]
    );

    let sloppy = compile("function g(a = eval('var z = 1')) { return z; }");
    let g = function(&sloppy, "g");
    assert_eq!(g.scopes[0].kind, ScopeKind::Callee);
    assert!(g.scopes[0].flags.has_extension && g.scopes[0].flags.var_scope);
    assert!(g.scopes[0].slots.is_empty());
}

#[test]
fn named_function_expression_binds_itself_with_load_self() {
    let module = compile("var f = function g() { return () => g; };");
    let g = function(&module, "g");
    assert_eq!(g.scopes[0].kind, ScopeKind::FunctionName);
    assert_eq!(g.scopes[0].slots[0].kind, SlotKind::FnSelfName);
    let code = ops(g);
    assert!(code.contains(&Op::LoadSelf), "{code:?}");
    assert!(!code.contains(&Op::MakeFunction));
    let direct = compile("var f = function g(n) { return n ? g(n - 1) : 0; };");
    let g = function(&direct, "g");
    // Not captured: the self binding is a register filled by LoadSelf.
    assert!(g.scopes.is_empty());
    assert!(ops(g).contains(&Op::LoadSelf));
}

#[test]
fn derived_this_observed_by_an_arrow_lives_in_a_slot() {
    let module = compile(
        "class A {} class B extends A { constructor() { const f = () => this; super(); \
         f(); if (f()) return; } }",
    );
    let ctor = module
        .functions
        .iter()
        .find(|function| function.name == "B" && function.is_derived_constructor)
        .unwrap();
    let this_scope = ctor
        .scopes
        .iter()
        .find(|scope| {
            scope
                .slots
                .iter()
                .any(|slot| slot.kind == SlotKind::DerivedThis)
        })
        .expect("DerivedThis slot");
    assert_eq!(slot_names(this_scope)[0], "this");
    let code = ops(ctor);
    assert!(code.contains(&Op::BindThisContextSlot), "{code:?}");
    assert!(code.contains(&Op::ReturnDerived));
    assert!(
        !code
            .iter()
            .any(|op| matches!(op, Op::Return | Op::ReturnValue | Op::ReturnUndefined)),
        "{code:?}"
    );
    let arrow = module
        .functions
        .iter()
        .find(|function| function.is_arrow)
        .unwrap()
        .name
        .clone();
    assert_eq!(
        slot_accesses(&module, &arrow, Op::LoadContextSlotChecked),
        [(0, "this".to_string())]
    );

    // No arrow, eval, or arrow super(): the frame keeps `this`.
    let plain = compile("class A {} class C extends A { constructor() { super(); this.x = 1; } }");
    let ctor = plain
        .functions
        .iter()
        .find(|function| function.name == "C" && function.is_derived_constructor)
        .unwrap();
    assert!(ctor.scopes.is_empty());
    assert!(ops(ctor).contains(&Op::BindThisValue));
}

#[test]
fn sloppy_eval_routes_through_extension_lookups() {
    let module = compile("function f() { eval('var x = 1'); return x; }");
    let f = function(&module, "f");
    assert_eq!(f.scopes[0].kind, ScopeKind::Body);
    assert!(f.scopes[0].flags.has_extension);
    let code = &f.code;
    let load = code
        .iter()
        .find(|instruction| instruction.op == Op::LoadLookupGlobal)
        .expect("lookup load");
    assert_eq!(imm(code.operand(load, 3)), Some(1));
    assert!(ops(f).contains(&Op::Eval));

    // A slot reached across an arrow's own extension probes it first.
    let module = compile("function g() { var y = 1; return () => { eval(''); return y; }; }");
    let arrow = module
        .functions
        .iter()
        .find(|function| function.is_arrow)
        .unwrap();
    assert!(arrow.scopes[0].flags.has_extension);
    assert!(ops(arrow).contains(&Op::LoadLookupSlot), "{:?}", ops(arrow));

    // Strict code never extends: plain slot accesses.
    let strict = compile("function h() { 'use strict'; var y = 1; eval(''); return y; }");
    let h = function(&strict, "h");
    assert!(!h.scopes[0].flags.has_extension);
    assert!(!ops(h).contains(&Op::LoadLookupSlot));
}

#[test]
fn eval_shadowable_assignment_resolves_before_the_rhs() {
    let module = compile("function f(g) { eval(''); x = g(); y += g(); return x; }");
    let f = function(&module, "f");
    let code = ops(f);
    let resolves: Vec<usize> = code
        .iter()
        .enumerate()
        .filter(|(_, op)| **op == Op::ResolveLookupRef)
        .map(|(pc, _)| pc)
        .collect();
    let stores: Vec<usize> = code
        .iter()
        .enumerate()
        .filter(|(_, op)| **op == Op::StoreRef)
        .map(|(pc, _)| pc)
        .collect();
    // Skip the guarded `eval(...)`'s shadowed-callee call.
    let calls: Vec<usize> = code
        .iter()
        .enumerate()
        .filter(|(pc, op)| **op == Op::Call && *pc > resolves[0])
        .map(|(pc, _)| pc)
        .collect();
    assert_eq!(resolves.len(), 2, "{code:?}");
    assert_eq!(stores.len(), 2);
    for ((resolve, store), call) in resolves.iter().zip(&stores).zip(&calls) {
        assert!(resolve < call && call < store, "{code:?}");
    }
    let store = f
        .code
        .iter()
        .find(|instruction| instruction.op == Op::StoreRef)
        .unwrap();
    let mode = StoreRefMode::from_imm32(imm(f.code.operand(store, 3)).unwrap()).unwrap();
    assert_eq!(
        mode.slot, None,
        "an unbound name resolves against the global"
    );
    assert!(!mode.strict);
}

#[test]
fn make_closure_names_the_innermost_context() {
    let module = compile("function f() { let x = 1; return () => x; }");
    let f = function(&module, "f");
    let code = &f.code;
    let create = code
        .iter()
        .find(|instruction| instruction.op == Op::CreateContext)
        .unwrap();
    let make = code
        .iter()
        .find(|instruction| instruction.op == Op::MakeClosure)
        .unwrap();
    assert_eq!(code.operand(make, 2), code.operand(create, 0));
}

#[test]
fn catch_parameters_and_with_objects_get_fresh_contexts() {
    let module = compile(
        "function f(fs, o) { for (var i = 0; i < 2; i++) { try { throw i; } catch (e) { \
         fs.push(() => e); } } with (o) { fs.push(() => x); } }",
    );
    let f = function(&module, "f");
    let catch = f
        .scopes
        .iter()
        .position(|scope| scope.kind == ScopeKind::Catch)
        .expect("catch scope");
    assert_eq!(
        f.scopes[catch].slots[0].kind,
        SlotKind::CatchParam { simple: true }
    );
    let with = f
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::With)
        .expect("with scope");
    assert_eq!(with.slots[0].kind, SlotKind::WithObject);
    // An uncaptured catch parameter stays a register.
    let plain = compile("function g() { try { throw 1; } catch (e) { return e; } }");
    assert!(function(&plain, "g").scopes.is_empty());
}

#[test]
fn try_block_functions_are_hoisted() {
    let module =
        compile("function f() { try { return g(); function g() { return 1; } } finally {} }");
    let f = function(&module, "f");
    let code = ops(f);
    let make = code
        .iter()
        .position(|op| matches!(op, Op::MakeFunction | Op::MakeClosure))
        .expect("hoisted function");
    let call = code.iter().position(|op| *op == Op::Call).expect("call");
    assert!(make < call, "{code:?}");
}

#[test]
fn module_exports_mirror_only_the_module_binding() {
    let host = ModuleHostInfo {
        module_url: "file:///test/m.js".to_string(),
        resolved_imports: Default::default(),
    };
    let module = with_program(
        "export let x = 1; export function f() { let x = 2; x = 3; return x; } \
         export function g() { x = 5; }",
        SyntaxSourceKind::JavaScript,
        |program| compile_module_program(program, SyntaxSourceKind::JavaScript, &host),
    )
    .unwrap()
    .unwrap();
    assert_verified(&module);
    let init = &module.functions[0];
    let exported: Vec<&SlotDescriptor> = init.scopes[0]
        .slots
        .iter()
        .filter(|slot| slot.exported)
        .collect();
    assert!(
        exported.iter().any(|slot| slot.name == "x"),
        "{:?}",
        init.scopes[0]
    );
    let stores_property = |name: &str| ops(function(&module, name)).contains(&Op::StoreProperty);
    assert!(!stores_property("f"), "a shadowing local never mirrors");
    assert!(stores_property("g"));
}

fn eval_chain(
    scopes: Vec<(ScopeKind, bool, Vec<(&str, SlotKind)>)>,
    var_depth: Option<u32>,
) -> EvalCallerChain {
    EvalCallerChain {
        scopes: scopes
            .into_iter()
            .map(|(kind, extension, slots)| EvalCallerScope {
                descriptor: ScopeDescriptor {
                    kind,
                    flags: ScopeFlags {
                        strict: false,
                        var_scope: kind == ScopeKind::Body,
                        has_extension: extension,
                    },
                    slots: slots
                        .into_iter()
                        .map(|(name, kind)| SlotDescriptor {
                            name: name.to_string(),
                            kind,
                            exported: false,
                        })
                        .collect(),
                },
                extension_names: Vec::new(),
            })
            .collect(),
        var_depth,
    }
}

fn compile_eval(
    source: &str,
    chain: Option<&EvalCallerChain>,
) -> Result<BytecodeModule, CompileError> {
    let module = compile_eval_source(
        source,
        SyntaxSourceKind::JavaScript,
        "eval",
        false,
        false,
        chain,
        true,
        false,
        false,
        false,
        false,
    )?;
    assert_verified_with_caller(&module, chain);
    Ok(module)
}

#[test]
fn direct_eval_resolves_through_the_caller_chain() {
    let chain = eval_chain(
        vec![
            (ScopeKind::Block, false, vec![("y", SlotKind::Let)]),
            (ScopeKind::Body, true, vec![("x", SlotKind::Var)]),
        ],
        Some(1),
    );
    let module = compile_eval(
        "var x = 1; var z = 2; y; w; function k() { return z + y; }",
        Some(&chain),
    )
    .unwrap();
    let main = module.main();
    let code = ops(main);
    assert_eq!(code.first(), Some(&Op::LoadClosureContext));
    // `z` has no static caller slot: a deletable extension binding.
    assert!(code.contains(&Op::DeclareEvalVar), "{code:?}");
    // The hoisted function lands in the caller's variable scope extension.
    assert!(code.contains(&Op::StoreVarScope), "{code:?}");
    // `x` re-binds the caller's var slot through the Block's hop.
    let checker = ChainChecker::new(&module, Some(&chain));
    let stores: Vec<String> = main
        .code
        .iter()
        .enumerate()
        .filter(|(_, instruction)| {
            matches!(instruction.op, Op::StoreContextSlot | Op::StoreLookupSlot)
        })
        .map(|(pc, instruction)| {
            let (ctx, coord) = if instruction.op == Op::StoreContextSlot {
                (
                    register(main.code.operand(instruction, 1)).unwrap(),
                    imm(main.code.operand(instruction, 2)).unwrap(),
                )
            } else {
                (
                    register(main.code.operand(instruction, 1)).unwrap(),
                    imm(main.code.operand(instruction, 3)).unwrap(),
                )
            };
            checker
                .slot_at(0, pc, ctx, ContextCoord::from_imm32(coord).unwrap())
                .unwrap()
                .name
                .clone()
        })
        .collect();
    assert!(stores.contains(&"x".to_string()), "{stores:?}");
    // `w` and `z` probe the extension, then the global object.
    assert!(code.contains(&Op::LoadLookupGlobal));
    let k = function(&module, "k");
    assert!(ops(k).contains(&Op::LoadLookupGlobal));
    assert!(ops(k).contains(&Op::LoadContextSlotChecked) || ops(k).contains(&Op::LoadLookupSlot));

    // §19.2.1.3 step 3 — a var over a caller lexical is a SyntaxError.
    let error = compile_eval("var y;", Some(&chain)).unwrap_err();
    assert!(format!("{error:?}").contains("already been declared"));
    // Annex B.3.4 — a simple catch parameter is exempt.
    let catch = eval_chain(
        vec![
            (
                ScopeKind::Catch,
                false,
                vec![("e", SlotKind::CatchParam { simple: true })],
            ),
            (ScopeKind::Body, true, vec![]),
        ],
        Some(1),
    );
    compile_eval("var e = 1;", Some(&catch)).unwrap();
    // Indirect eval: no caller context at all.
    let indirect = compile_eval("var q = 1; q", None).unwrap();
    assert!(!ops(indirect.main()).contains(&Op::LoadClosureContext));
}

#[test]
fn eval_in_a_derived_constructor_reads_the_this_slot() {
    let chain = eval_chain(
        vec![
            (
                ScopeKind::Body,
                false,
                vec![("this", SlotKind::DerivedThis)],
            ),
            (
                ScopeKind::Class,
                false,
                vec![
                    ("%home", SlotKind::SuperHome),
                    ("%super", SlotKind::SuperCtor),
                    ("%class", SlotKind::ClassSelf),
                ],
            ),
        ],
        Some(0),
    );
    let module = compile_eval_source(
        "super(); this",
        SyntaxSourceKind::JavaScript,
        "eval",
        true,
        false,
        Some(&chain),
        true,
        false,
        true,
        true,
        false,
    )
    .unwrap();
    assert_verified_with_caller(&module, Some(&chain));
    let code = ops(module.main());
    assert!(code.contains(&Op::BindThisContextSlot), "{code:?}");
    assert!(code.contains(&Op::LoadContextSlotChecked));
    assert!(!code.contains(&Op::LoadThis));
}

#[test]
fn difftest_corpus_compiles_to_verified_bytecode() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../otter-difftest/corpus");
    let mut entries: Vec<_> = std::fs::read_dir(&corpus)
        .expect("difftest corpus")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "js"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty());
    for path in entries {
        let source = std::fs::read_to_string(&path).unwrap();
        let module = compile_script_source(&source, SyntaxSourceKind::JavaScript, "corpus.js")
            .unwrap_or_else(|error| panic!("{}: {error:?}", path.display()));
        if let Err(error) = otter_bytecode::verify_module(&module) {
            panic!("{}: {error:?}", path.display());
        }
        if let Err(error) = ChainChecker::new(&module, None).check() {
            panic!("{}: {error}", path.display());
        }
    }
}
