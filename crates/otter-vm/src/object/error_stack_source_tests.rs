//! Accounted managed source arenas and fallible owned diagnostic extraction.
//!
//! # Contents
//! - Repeated full source lines are part of the exact GC variable-cell extent.
//! - The sole receiver/alias handles survive the cap-admission collection of
//!   a real publication.
//! - Borrowed formatting allocates no source copies; owned extraction admits each copy.
//! - A source-account refusal leaves the published managed arena unchanged.
//!
//! # Invariants
//! - Heap values use the standard interpreter handle scope and current slots.
//! - Arena records/UTF-8 bytes retain no code function liveness or Rust source lease.
//! - Assertions require exact receiver relocation and the canonical heap ledger.
//!
//! # See also
//! - `super::ErrorStackBody` owns the managed immutable arena.
//! - `super::visit_error_stack_frames` owns borrowed diagnostic traversal.

use super::*;
use crate::{Interpreter, StackFrameSnapshot};
use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits, SharedSource};

fn usage(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

#[test]
fn managed_source_arena_collects_roots_charges_exact_extent_and_extracts_fallibly() {
    let source_account = ResourceAccount::default();
    let text = format!("{}é𝄞", "long captured line ".repeat(8192));
    let source = SharedSource::admit(&source_account, text.clone()).unwrap();
    let mut vm = Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("arena interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_object(scope).expect("fresh arena receiver");
        let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
        let mut current = vm.escape_scoped(owner).as_object().unwrap();
        ensure_exotic(&mut current, &mut vm.gc_heap).unwrap();
        let before_offset = current.offset();
        let frames: Vec<_> = (0..3)
            .map(|index| StackFrameSnapshot {
                function_id: 123 + index,
                function_name: format!("frame{index}"),
                module: "captured.js".to_owned(),
                span: (1, 2),
                source_position: Some(crate::ErrorSourcePosition {
                    script_name: "captured.js".to_owned(),
                    line_number: 7,
                    start_column: 3,
                    source_line: source.clone(),
                }),
            })
            .collect();
        let arena_bytes: usize = frames
            .iter()
            .map(|frame| frame.function_name.len() + frame.module.len() + source.len())
            .sum();
        let unaligned = std::mem::size_of::<otter_gc::GcHeader>()
            + std::mem::size_of::<ErrorStackBody>()
            + ErrorStackBody::trailing_bytes(frames.len(), arena_bytes);
        let physical =
            (unaligned + otter_gc::OBJECT_ALIGNMENT - 1) & !(otter_gc::OBJECT_ALIGNMENT - 1);
        // The arena is an old-space variable body: its only collecting point
        // is cap admission, which runs one emergency full collection (with
        // its scavenge) before refusing. Fill the remaining headroom with an
        // unreachable string so publication cannot be admitted without that
        // actual collection, which must then reclaim the garbage and keep the
        // receiver rooted through the publication's own root visitor.
        let headroom = vm.gc_heap.max_heap_bytes() - vm.gc_heap.stats().tracked_bytes;
        let garbage = usize::try_from(headroom).unwrap() - physical + 4096;
        crate::JsString::from_str(&"g".repeat(garbage), &mut vm.gc_heap)
            .expect("unreachable cap filler");
        assert!(
            vm.gc_heap.stats().tracked_bytes + physical as u64 > vm.gc_heap.max_heap_bytes(),
            "publication must exceed the cap before collecting"
        );
        let cycles = vm.gc_heap.gc_cycle_counts();
        set_error_stack_frames(&mut current, &mut vm.gc_heap, frames)
            .expect("collecting arena publication");
        assert!(vm.gc_heap.gc_cycle_counts().0 > cycles.0);
        assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
        assert_ne!(
            current.offset(),
            before_offset,
            "actual fresh receiver moved during arena allocation"
        );
        assert_eq!(Value::object(current), vm.escape_scoped(owner));
        assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
        let stack = vm
            .gc_heap
            .read_payload(current, |body| body.exotic().unwrap().error_stack_frames);
        let actual_size = vm.gc_heap.read_payload(stack, |body| {
            assert_eq!(body.frame_count, 3);
            assert_eq!(body.byte_len, arena_bytes);
            std::mem::size_of::<otter_gc::GcHeader>()
                + std::mem::size_of::<ErrorStackBody>()
                + ErrorStackBody::trailing_bytes(body.frame_count, body.byte_len)
        });
        assert_eq!(
            (actual_size + otter_gc::OBJECT_ALIGNMENT - 1) & !(otter_gc::OBJECT_ALIGNMENT - 1),
            physical
        );
        let heap_stats = vm.gc_heap.stats();
        assert_eq!(
            heap_stats.tracked_bytes,
            heap_stats.allocated_bytes as u64 + heap_stats.reserved_bytes
        );
        let before = usage(&source_account);
        let mut seen = 0;
        assert!(visit_error_stack_frames(
            current,
            &vm.gc_heap,
            |name, module, position| {
                assert_eq!(name, format!("frame{seen}"));
                assert_eq!(module, "captured.js");
                assert_eq!(position, Some((7, 3)));
                seen += 1;
            }
        ));
        assert_eq!(seen, 3);
        assert_eq!(
            usage(&source_account),
            before,
            "borrowed diagnostics create no owned source copies"
        );
        let extracted_account = ResourceAccount::default();
        let copied = error_stack_frames(current, &vm.gc_heap, &extracted_account)
            .unwrap()
            .unwrap();
        assert_eq!(usage(&extracted_account), 3 * text.len() as u64);
        for frame in &copied {
            assert_eq!(
                frame.source_position.as_ref().unwrap().source_line.as_ref(),
                text
            );
        }
        let copy_alias = copied.clone();
        assert_eq!(usage(&extracted_account), 3 * text.len() as u64);
        drop(copied);
        drop(copy_alias);
        assert_eq!(usage(&extracted_account), 0);
        let refused = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::SourceModuleBytes, 0)
                .build(),
        );
        assert!(matches!(
            error_stack_frames(current, &vm.gc_heap, &refused),
            Err(otter_resource::SharedSourceError::Resource(_))
        ));
        assert_eq!(usage(&refused), 0);
        assert_eq!(
            vm.gc_heap
                .read_payload(current, |body| body.exotic().unwrap().error_stack_frames),
            stack
        );
        vm.force_gc()
            .expect("retain managed arena after owned refusal");
        let current = vm.escape_scoped(owner).as_object().unwrap();
        let final_copy = error_stack_frames(current, &vm.gc_heap, &extracted_account)
            .unwrap()
            .unwrap();
        assert_eq!(final_copy.len(), 3);
        assert_eq!(
            final_copy[0]
                .source_position
                .as_ref()
                .unwrap()
                .source_line
                .as_ref(),
            text
        );
        drop(final_copy);
        assert_eq!(usage(&extracted_account), 0);
    });
    assert_eq!(usage(&source_account), text.len() as u64);
    drop(source);
    assert_eq!(usage(&source_account), 0);
}
