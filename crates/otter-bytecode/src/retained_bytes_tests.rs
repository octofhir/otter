//! Physical retained compiler capacity used by code-space admission.
//!
//! # Contents
//! - Spare module rows, nested vectors, strings and handlers are counted.
//! - Boxed wordcode geometry remains exactly its immutable arrays.
//!
//! # Invariants
//! Retention follows actual allocated capacity; clearing a logical row never
//! silently refunds memory its Vec/String still owns.

use super::*;

#[test]
fn retained_module_counts_spare_rows_nested_strings_and_exception_capacity() {
    let mut module = BytecodeModule {
        module: String::new(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: Vec::new(),
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    assert_eq!(module.retained_bytes(), 0);
    module.module.try_reserve_exact(129).unwrap();
    module.functions.try_reserve_exact(3).unwrap();
    module.constants.try_reserve_exact(5).unwrap();
    let base = module.module.capacity() as u64
        + (module.functions.capacity() * std::mem::size_of::<Function>()) as u64
        + (module.constants.capacity() * std::mem::size_of::<Constant>()) as u64;
    assert_eq!(module.retained_bytes(), base);
    let mut function = Function::default();
    function.name.try_reserve_exact(67).unwrap();
    function.handlers.try_reserve_exact(7).unwrap();
    function.scopes.try_reserve_exact(2).unwrap();
    let mut scope = ScopeDescriptor {
        kind: ScopeKind::Body,
        flags: ScopeFlags::default(),
        slots: Vec::new(),
    };
    scope.slots.try_reserve_exact(4).unwrap();
    let mut name = String::new();
    name.try_reserve_exact(83).unwrap();
    name.push_str("x");
    let name_capacity = name.capacity();
    scope.slots.push(SlotDescriptor {
        name,
        kind: SlotKind::Var,
        exported: false,
    });
    let scope_bytes =
        (scope.slots.capacity() * std::mem::size_of::<SlotDescriptor>() + name_capacity) as u64;
    assert_eq!(scope.retained_bytes(), scope_bytes);
    let expected = function.name.capacity() as u64
        + (function.handlers.capacity() * std::mem::size_of::<ExceptionHandler>()) as u64
        + (function.scopes.capacity() * std::mem::size_of::<ScopeDescriptor>()) as u64
        + scope_bytes;
    function.scopes.push(scope);
    assert_eq!(function.retained_bytes(), expected);
    let handler_bytes =
        (function.handlers.capacity() * std::mem::size_of::<ExceptionHandler>()) as u64;
    function.handlers.clear();
    assert_eq!(
        function.retained_bytes(),
        expected,
        "clearing an empty table does not release its physical buffer"
    );
    module.functions.push(function);
    assert_eq!(module.retained_bytes(), base + expected);
    module.functions[0].handlers.shrink_to_fit();
    assert_eq!(module.retained_bytes(), base + expected - handler_bytes);
}
