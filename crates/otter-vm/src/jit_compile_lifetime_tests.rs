//! Prepared source leases and exact optimizing-publication identity.
//!
//! # Contents
//! - Recursive plain and canonical guarded-method source ownership.
//! - Physical eviction eligibility before and after source-lease release.
//! - Foreign CodeSpace adoption with overlapping function identifiers.
//!
//! # Invariants
//! - Fixtures link real verified chunks; no runtime or compiler thread is needed.
//! - Snapshots retain CodeBlocks after eviction, but cannot retain ChunkPayloads.
//! - Only saved source owners may satisfy an actual spliced-source declaration.
//!
//! # See also
//! - Queue tests own terminal-ACK ordering and active compilation cancellation.
//! - `code_space::source_tests` isolates source text versus executable lifetime.

use super::*;
use crate::code_space::{ChunkEvictionResult, ChunkResolution, CodeSpace};
use crate::jit::{
    JitCompileSnapshot, JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitInlineCallee,
    JitMethodGuard, JitMethodInlineFields, JitMethodTarget,
};
use crate::native_abi::NativeFrameKind;
use crate::object::FieldLocation;
use crate::source_registry::SourceRegistry;
use otter_resource::ResourceAccount;
use std::sync::Arc;

fn linked_source(
    space: &Arc<CodeSpace>,
    account: &ResourceAccount,
    name: &str,
) -> ExecutionContext {
    space
        .link_evictable_module(
            crate::test_support::minimal_bytecode_module(name),
            SourceRegistry::default(),
            account,
        )
        .expect("verified evictable source")
}

fn method(body: JitCompileSnapshot, call: bool, fields: bool) -> JitMethodTarget {
    JitMethodTarget {
        target_index: 0,
        target_count: 1,
        guard: JitMethodGuard {
            method_fid: body.code_block.id,
            recv_shape: 0,
            prototype_validity: None,
            holder_root: 0,
            method_field: FieldLocation::inline(0),
            slot_proven: false,
            method_cell: 0,
            accessor_byte: None,
        },
        callee: call.then_some(JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: body.code_block.id,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: 0,
                callee_cell: 0,
            },
            receiver_allocation: None,
        }),
        body: Some(Arc::new(body)),
        inline_fields: fields.then_some(JitMethodInlineFields {
            prop_fields: Default::default(),
            prop_shapes: Default::default(),
        }),
    }
}

#[test]
fn cancellation_sources_hold_payloads_until_release_and_reject_foreign_adoption() {
    let account = ResourceAccount::default();
    let space = Arc::new(CodeSpace::default());
    let root_context = linked_source(&space, &account, "compile-root.js");
    let plain_context = linked_source(&space, &account, "compile-plain.js");
    let method_context = linked_source(&space, &account, "compile-method.js");
    let poly_context = linked_source(&space, &account, "compile-poly.js");
    let direct_context = linked_source(&space, &account, "compile-direct-only.js");
    let mut root = root_context
        .jit_compile_snapshot(root_context.function_base())
        .unwrap();
    let root_only = root.clone();
    let mut plain = plain_context
        .jit_compile_snapshot(plain_context.function_base())
        .unwrap();
    let mut nested_method = method_context
        .jit_compile_snapshot(method_context.function_base())
        .unwrap();
    let mut poly = poly_context
        .jit_compile_snapshot(poly_context.function_base())
        .unwrap();
    let direct = direct_context
        .jit_compile_snapshot(direct_context.function_base())
        .unwrap();
    let spliced = [
        plain.code_block.id,
        nested_method.code_block.id,
        poly.code_block.id,
        direct.code_block.id,
    ];
    // All capabilities use the one source edge: combined call/leaf, Graph-only,
    // and a Template leaf with no ready call generation, in independent chunks.
    poly.method_targets
        .insert(0, vec![method(direct, true, true)]);
    nested_method
        .method_targets
        .insert(0, vec![method(poly, true, false)]);
    plain
        .method_targets
        .insert(0, vec![method(nested_method, false, true)]);
    root.inline_callees.insert(
        0,
        JitInlineCallee {
            body: Arc::new(plain),
        },
    );

    let sources = CompilationSources::retain(&root_context, &root)
        .expect("every recursively prepared body resolves to its exact source");
    let own_source = CompilationSources::retain(&root_context, &root_only).unwrap();
    assert!(sources.is_current(&space, &root.code_block, &spliced));
    assert!(own_source.is_current(&space, &root.code_block, &[]));
    assert!(
        !own_source.is_current(&space, &root.code_block, &spliced),
        "a live but unretained source cannot satisfy an actual splice"
    );
    assert!(!sources.is_current(&space, &root.code_block, &[u32::MAX]));

    // A separately adopted directory can reuse every dense identifier. Its
    // entries are all Live, but they are different physical source owners.
    let foreign = Arc::new(CodeSpace::default());
    let foreign_root = linked_source(&foreign, &account, "adopted-root.js");
    let foreign_plain = linked_source(&foreign, &account, "adopted-plain.js");
    let foreign_method = linked_source(&foreign, &account, "adopted-method.js");
    let foreign_poly = linked_source(&foreign, &account, "adopted-poly.js");
    let foreign_direct = linked_source(&foreign, &account, "adopted-direct-only.js");
    assert_eq!(foreign_root.function_base(), root.code_block.id);
    for &fid in &spliced {
        assert!(matches!(
            foreign.resolve_chunk(fid),
            ChunkResolution::Live { .. }
        ));
    }
    assert!(!sources.is_current(&foreign, &root.code_block, &spliced));
    let foreign_body = foreign_root
        .jit_compile_snapshot(foreign_root.function_base())
        .unwrap();
    assert!(
        !sources.is_current(&space, &foreign_body.code_block, &spliced),
        "even the original directory cannot admit a different same-id CodeBlock"
    );
    assert!(CompilationSources::retain(&foreign_root, &root).is_none());
    drop((
        foreign_root,
        foreign_plain,
        foreign_method,
        foreign_poly,
        foreign_direct,
    ));

    // This is the physical ownership used while a worker has not ACKed yet.
    // CodeBlock Arcs in `root` remain throughout; only the explicit source
    // leases prevent payload reclamation once the caller's contexts disappear.
    drop((
        root_context,
        plain_context,
        method_context,
        poly_context,
        direct_context,
    ));
    assert!(space.eviction_candidates().is_empty());
    drop(sources);
    let nested_candidates = space.eviction_candidates();
    assert_eq!(nested_candidates.len(), 4);
    for candidate in nested_candidates {
        assert!(spliced.contains(&candidate.function_base));
        assert_eq!(
            space.evict_candidate(candidate),
            ChunkEvictionResult::Evicted {
                retained_bytes: candidate.retained_bytes,
            }
        );
    }
    assert!(own_source.is_current(&space, &root.code_block, &[]));
    assert!(!own_source.is_current(&space, &root.code_block, &spliced));
    for &fid in &spliced {
        assert!(matches!(
            space.resolve_chunk(fid),
            ChunkResolution::Evicted { .. }
        ));
    }
    drop(own_source);
    let candidates = space.eviction_candidates();
    let [root_candidate] = candidates.as_slice() else {
        panic!("the final source payload is eligible after the last lease drops");
    };
    assert_eq!(root_candidate.function_base, root.code_block.id);
    assert!(matches!(
        space.evict_candidate(*root_candidate),
        ChunkEvictionResult::Evicted { .. }
    ));
    assert_eq!(space.evictable_retained_bytes(), 0);
}
