//! Machine-readable dispatch metadata for host callables.
//!
//! # Contents
//! - [`NativeCallHeader`] owns callable identity, entry payload and policy.
//! - [`NativeEntryKind`] selects the payload's interpretation.
//! - Typed access to static entries, VM intrinsics and host-reference indices.
//!
//! # Invariants
//! Allocation initializes the complete header before publishing a body. Entry
//! kind and payload never change; property mutations change only policy bits.
//! The payload contains no moving pointers or values and needs no tracing.
//! A typed union member is read only after checking its immutable kind. Host
//! closures stay in the isolate's host-reference table, with one owning body.
//!
//! # See also
//! - [`super::NativeFunctionBody`] owns the traced captures after this header.
//! - [`crate::jit::JitNativeCallLayout`] describes this one layout to both JITs.

use super::{
    LocalNativeFn, NativeCallStorage, NativeCallTarget, NativeCapturesFn, NativeFastFn, NativeFn,
    NativeFunctionMetadata, VmIntrinsicFunction,
};
use std::sync::Arc;

/// Fixed discriminants used by generated native-callee dispatch.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeEntryKind {
    /// NativeCtx function with no captures.
    Static = 0,
    /// NativeCtx function with a traced capture slab.
    StaticWithCaptures = 1,
    /// VM operation requiring JavaScript activation access.
    VmIntrinsic = 2,
    /// Shared host closure retained by a host-reference index.
    Dynamic = 3,
    /// Isolate-local host closure retained by a host-reference index.
    LocalDynamic = 4,
}

#[repr(C)]
#[derive(Clone, Copy)]
union NativeEntryPayload {
    static_entry: NativeFastFn,
    captures_entry: NativeCapturesFn,
    bits: u64,
}

/// Authoritative non-GC prefix of every native callable body.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NativeCallHeader {
    pub(super) native_ref: u32,
    pub(super) kind: NativeEntryKind,
    flags: u8,
    pub(super) length: u8,
    reserved: u8,
    payload: NativeEntryPayload,
}

impl NativeCallHeader {
    pub(super) const NAME_CONFIGURABLE: u8 = 1 << 0;
    pub(super) const LENGTH_CONFIGURABLE: u8 = 1 << 1;
    pub(super) const CONSTRUCTABLE: u8 = 1 << 2;
    pub(super) const EXTENSIBLE: u8 = 1 << 3;

    pub(super) fn allocate(
        heap: &mut otter_gc::GcHeap,
        call: NativeCallStorage,
        metadata: NativeFunctionMetadata,
        length: u8,
    ) -> Self {
        let native_ref = heap.intern_external_ref(match &call {
            NativeCallStorage::Static(f) => *f as *const () as usize,
            NativeCallStorage::StaticWithCaptures(f) => *f as *const () as usize,
            NativeCallStorage::VmIntrinsic(intrinsic) => intrinsic.identity_address(),
            NativeCallStorage::Dynamic(_) | NativeCallStorage::LocalDynamic(_) => 0,
        });
        let (kind, payload) = match call {
            NativeCallStorage::Static(f) => (
                NativeEntryKind::Static,
                NativeEntryPayload { static_entry: f },
            ),
            NativeCallStorage::StaticWithCaptures(f) => (
                NativeEntryKind::StaticWithCaptures,
                NativeEntryPayload { captures_entry: f },
            ),
            NativeCallStorage::VmIntrinsic(intrinsic) => (
                NativeEntryKind::VmIntrinsic,
                NativeEntryPayload {
                    bits: intrinsic as u64,
                },
            ),
            NativeCallStorage::Dynamic(arc) => (
                NativeEntryKind::Dynamic,
                NativeEntryPayload {
                    bits: u64::from(heap.intern_host_ref(Box::new(arc))),
                },
            ),
            NativeCallStorage::LocalDynamic(arc) => (
                NativeEntryKind::LocalDynamic,
                NativeEntryPayload {
                    bits: u64::from(heap.intern_host_ref(Box::new(arc))),
                },
            ),
        };
        Self {
            native_ref,
            kind,
            flags: (u8::from(metadata.name_configurable) * Self::NAME_CONFIGURABLE)
                | (u8::from(metadata.length_configurable) * Self::LENGTH_CONFIGURABLE)
                | (u8::from(metadata.constructable) * Self::CONSTRUCTABLE)
                | (u8::from(metadata.extensible) * Self::EXTENSIBLE),
            length,
            reserved: 0,
            payload,
        }
    }

    pub(super) const fn name_configurable(self) -> bool {
        self.flags & Self::NAME_CONFIGURABLE != 0
    }

    pub(super) const fn length_configurable(self) -> bool {
        self.flags & Self::LENGTH_CONFIGURABLE != 0
    }

    pub(super) const fn constructable(self) -> bool {
        self.flags & Self::CONSTRUCTABLE != 0
    }

    pub(super) const fn extensible(self) -> bool {
        self.flags & Self::EXTENSIBLE != 0
    }

    pub(super) fn prevent_extensions(&mut self) {
        self.flags &= !Self::EXTENSIBLE;
    }

    pub(super) fn host_ref(self) -> Option<u32> {
        match self.kind {
            NativeEntryKind::Dynamic | NativeEntryKind::LocalDynamic => {
                // SAFETY: allocation initializes all eight bytes for index payloads.
                Some(unsafe { self.payload.bits } as u32)
            }
            _ => None,
        }
    }

    pub(super) fn static_fn(self) -> Option<NativeFastFn> {
        (self.kind == NativeEntryKind::Static).then(|| {
            // SAFETY: allocation wrote this exact typed member for Static.
            unsafe { self.payload.static_entry }
        })
    }

    pub(super) fn static_address(self) -> Option<usize> {
        match self.kind {
            NativeEntryKind::Static => {
                // SAFETY: immutable kind selects the member written at allocation.
                Some(unsafe { self.payload.static_entry } as *const () as usize)
            }
            NativeEntryKind::StaticWithCaptures => {
                // SAFETY: immutable kind selects the member written at allocation.
                Some(unsafe { self.payload.captures_entry } as *const () as usize)
            }
            _ => None,
        }
    }

    pub(super) fn intrinsic(self) -> Option<VmIntrinsicFunction> {
        if self.kind != NativeEntryKind::VmIntrinsic {
            return None;
        }
        // SAFETY: allocation writes a fully initialized numeric discriminant.
        Some(match unsafe { self.payload.bits } {
            0 => VmIntrinsicFunction::FunctionPrototypeCall,
            1 => VmIntrinsicFunction::FunctionPrototypeApply,
            2 => VmIntrinsicFunction::FunctionPrototypeBind,
            3 => VmIntrinsicFunction::FunctionPrototypeToString,
            4 => VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance,
            _ => unreachable!("native intrinsic payload was initialized by allocation"),
        })
    }

    pub(super) fn target(
        self,
        heap: &otter_gc::GcHeap,
        captures: crate::value_slab::ValueSlabHandle,
    ) -> NativeCallTarget {
        match self.kind {
            NativeEntryKind::Static => {
                NativeCallTarget::Static(self.static_fn().expect("static entry kind"))
            }
            NativeEntryKind::StaticWithCaptures => NativeCallTarget::StaticWithCaptures {
                // SAFETY: immutable kind selects the member written at allocation.
                call: unsafe { self.payload.captures_entry },
                captures,
            },
            NativeEntryKind::VmIntrinsic => {
                NativeCallTarget::VmIntrinsic(self.intrinsic().expect("intrinsic entry kind"))
            }
            NativeEntryKind::Dynamic => NativeCallTarget::Dynamic {
                call: heap
                    .host_refs()
                    .get(self.host_ref().expect("host-reference entry kind"))
                    .and_then(|any| any.downcast_ref::<Arc<NativeFn>>())
                    .cloned()
                    .expect("dynamic native's host-ref index resolves to its closure"),
                captures,
            },
            NativeEntryKind::LocalDynamic => NativeCallTarget::LocalDynamic {
                call: heap
                    .host_refs()
                    .get(self.host_ref().expect("host-reference entry kind"))
                    .and_then(|any| any.downcast_ref::<Arc<LocalNativeFn>>())
                    .cloned()
                    .expect("local dynamic native's host-ref index resolves to its closure"),
                captures,
            },
        }
    }
}

pub(super) const NATIVE_REF_OFFSET: usize = std::mem::offset_of!(NativeCallHeader, native_ref);
pub(super) const KIND_OFFSET: usize = std::mem::offset_of!(NativeCallHeader, kind);
pub(super) const FLAGS_OFFSET: usize = std::mem::offset_of!(NativeCallHeader, flags);
pub(super) const PAYLOAD_OFFSET: usize = std::mem::offset_of!(NativeCallHeader, payload);

const _: () = {
    assert!(std::mem::size_of::<NativeCallHeader>() == 16);
    assert!(NATIVE_REF_OFFSET == 0);
    assert!(KIND_OFFSET == 4);
    assert!(FLAGS_OFFSET == 5);
    assert!(PAYLOAD_OFFSET == 8);
};
