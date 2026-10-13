//! Installed generations' subscriptions to monotone shape facts.
//!
//! # Contents
//! - Installation verifies each fact, requires a flag, and subscribes it.
//! - A writer that breaks a fact sets the flag and makes the generation
//!   stale for entry; the fired drain invalidates it once, and retirement
//!   drops the shape's subscription list.
//!
//! # Invariants
//! These metadata fixtures never execute code. The interpreter fixture keeps
//! the subscribed shapes alive for the whole test.
//!
//! # See also
//! - [`crate::code_dependencies`] for subscription and firing.

use super::*;
use crate::code_dependencies::DependencyFlag;
use crate::native_abi::CodeObjectMetadata;
use crate::{Interpreter, Value, object};

#[derive(Debug)]
struct SubscribedCode {
    id: u64,
    dependencies: Box<[CodeDependency]>,
    flag: Option<Arc<DependencyFlag>>,
}

impl JitFunctionCode for SubscribedCode {
    fn metadata(&self) -> CodeObjectMetadata {
        CodeObjectMetadata {
            id: self.id,
            code_block_id: 0,
            entry_offset: 0,
            code_size: 4,
            safepoint_count: 0,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: self.dependencies.len() as u32,
        }
    }

    fn dependencies(&self) -> &[CodeDependency] {
        &self.dependencies
    }

    fn dependency_flag(&self) -> Option<&Arc<DependencyFlag>> {
        self.flag.as_ref()
    }

    fn code_len(&self) -> usize {
        4
    }

    fn entry_addr(&self) -> Option<usize> {
        Some(0x1000 + self.id as usize * 16)
    }
}

#[test]
fn installation_verifies_subscribes_and_retirement_unsubscribes() {
    let mut interpreter = Interpreter::new().expect("fixture interpreter");
    interpreter.with_handle_scope(|interpreter, scope| {
        let handle = interpreter.scoped_object_bare(scope).expect("object");
        let mut target = interpreter.escape_scoped(handle).as_object().unwrap();
        interpreter
            .create_data_property(&mut target, "x", Value::NULL)
            .unwrap();
        let shape = object::shape(target, interpreter.gc_heap());
        let stable = CodeDependency {
            kind: CodeDependencyKind::ShapeStable,
            identity: shape.offset(),
            expected: 1,
        };
        let summary = CodeDependency {
            kind: CodeDependencyKind::FieldType,
            identity: shape.offset(),
            expected: u64::from(crate::object::field_type::snapshot(shape) as u8),
        };
        let mut registry = JitCodeRegistry::new_boxed();
        let code = |id, flag: Option<Arc<DependencyFlag>>| {
            Arc::new(SubscribedCode {
                id,
                dependencies: Box::new([stable, summary]),
                flag,
            })
        };

        // A generation relying on facts it does not re-read needs a flag.
        assert_eq!(
            registry.register(300, code(300, None)),
            Err(JitInstallError::InvalidCode)
        );
        let flag = registry.dependency_flag();
        let installed = code(301, Some(flag.clone()));
        registry
            .register(301, installed.clone())
            .expect("current facts install");
        assert!(registry.is_current_for_entry(installed.as_ref()));
        assert!(!crate::object::shape_body::watchers(shape).is_null());

        // Leaving the shape fires the generation and stales it for entry.
        interpreter
            .create_data_property(&mut target, "y", Value::NULL)
            .unwrap();
        assert!(flag.is_set());
        assert!(!registry.is_current_for_entry(installed.as_ref()));
        // The drain retires the fired generation once.
        assert_eq!(registry.invalidate_fired(), [0]);
        assert!(registry.invalidate_fired().is_empty());

        // A broken fact makes a later installation stale.
        assert_eq!(
            registry.register(302, code(302, Some(DependencyFlag::new()))),
            Err(JitInstallError::DependencyChanged)
        );

        drop(installed);
        assert_eq!(registry.retire_unreferenced(), 1);
        assert!(crate::object::shape_body::watchers(shape).is_null());
    });
}

#[test]
fn context_slot_dependencies_install_while_watched_and_retire_their_subscriptions() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let context = crate::context::alloc_context_with_roots(
        &mut heap,
        crate::context::ContextShape {
            scope_function_id: 9,
            scope_index: 0,
            slot_count: 2,
            has_extension: false,
        },
        Value::undefined(),
        |_| false,
        &mut |_| {},
    )
    .expect("context");
    let mut registry = JitCodeRegistry::new_boxed();
    let watch = registry
        .watch_context_slot(&mut heap, context, 0)
        .expect("slot 0 watched");
    assert_eq!(
        registry.watch_context_slot(&mut heap, context, 1),
        Some(watch)
    );
    let slot = |slot: u16| CodeDependency {
        kind: CodeDependencyKind::ContextSlot,
        identity: watch,
        expected: u64::from(slot),
    };
    let code = |id, dependency, flag| {
        Arc::new(SubscribedCode {
            id,
            dependencies: Box::new([dependency]),
            flag: Some(flag),
        })
    };

    let flag = registry.dependency_flag();
    let installed = code(310, slot(0), flag.clone());
    registry
        .register(310, installed.clone())
        .expect("a watched slot installs");
    assert!(registry.is_current_for_entry(installed.as_ref()));

    // Writing the slot fires the generation and stales it for entry.
    assert!(crate::context::write_slot(
        &mut heap,
        context,
        0,
        Value::number_i32(4)
    ));
    assert!(flag.is_set());
    assert!(!registry.is_current_for_entry(installed.as_ref()));
    assert_eq!(registry.invalidate_fired(), [0]);

    // A compile that read the slot before the write installs stale.
    let late = code(311, slot(0), registry.dependency_flag());
    assert_eq!(
        registry.register(311, late),
        Err(JitInstallError::DependencyChanged)
    );

    // An invalidated generation's subscription goes with its retirement.
    let other = registry.dependency_flag();
    let retired = code(312, slot(1), other.clone());
    registry
        .register(312, retired.clone())
        .expect("slot 1 is still watched");
    registry.invalidate_code_object(312);
    drop((installed, retired));
    assert_eq!(registry.retire_unreferenced(), 2);
    assert!(crate::context::write_slot(
        &mut heap,
        context,
        1,
        Value::number_i32(5)
    ));
    assert!(!other.is_set());
}
