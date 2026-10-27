//! Primitive string operations shared by both compiled tiers and targets.
//!
//! # Contents
//! - Scoped collecting concatenation over explicit Number/String operands,
//!   placed for the calling instruction's allocation site.
//! - A Number's decimal string through the isolate's cache.
//! - Pure UTF-16 or numeric primitive ordering using canonical VM algorithms.
//!
//! # Invariants
//! A collecting entry roots its ABI operands and current safepoint, then builds
//! strings through the production handle arena. No raw value crosses a GC.
//! No object coercion, JS or property protocol runs inside these Probes.
//! Ordering never allocates in the managed heap, flattens a body or fills caches;
//! the existing general numeric parser may use temporary Rust text storage.
//! The third concat operand is `undefined` or the Int32 id of the site whose
//! current decision places every string the concatenation creates.
//! Rejected tags and logical length overflow are pre-effect Misses; canonical
//! arithmetic owns coercion order and catchable errors after eager resumption.
//!
//! # See also
//! - `crate::string::gc_body` owns content and rope hash/depth invariants.
//! - `crate::number::parse` is the one StringNumericLiteral parser.

use super::*;

/// Concatenate explicit admitted primitives using current typed safepoint roots.
/// `site` is `undefined` or the Int32 id of the instruction's allocation site.
#[must_use]
pub extern "C" fn string_concat_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    lhs: u64,
    rhs: u64,
    site: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(ctx, concat(ctx, safepoint, [lhs, rhs, site]))
}

/// `(number, _, _)` as boxed words: the Number's decimal string from the
/// isolate's cache, converted and cached on a miss. Any other value misses
/// before an effect.
#[must_use]
pub extern "C" fn number_to_string_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    number: u64,
    padding0: u64,
    padding1: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        number_to_string(ctx, safepoint, [number, padding0, padding1]),
    )
}

fn number_to_string(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    bits: [u64; 3],
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(vm) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let values = bits.map(Value::from_abi_bits);
    if !values[0].is_number() {
        return NativeResultPair::miss();
    }
    // SAFETY: exact current typed call packet and immutable safepoint remain
    // published until the conversion's allocation ends.
    let Ok(roots) = (unsafe { alloc_value_stub_call_roots(ctx, safepoint, values) }) else {
        return NativeResultPair::miss();
    };
    let _roots = vm
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    match vm.generated_number_string(roots.value(0)) {
        Ok(string) => NativeResultPair::success(Value::string(string)),
        Err(crate::VmError::OutOfMemory { .. }) => NativeResultPair::out_of_memory(),
        Err(_) => NativeResultPair::miss(),
    }
}

/// Placement of one concatenation at the site `value` names.
fn site_placement(heap: &otter_gc::GcHeap, value: Value) -> Option<otter_gc::SitePlacement> {
    use otter_gc::pretenuring::PretenureDecision;
    if value.is_undefined() {
        return Some(otter_gc::SitePlacement::Young(0));
    }
    let site = u16::try_from(value.as_i32()?).ok()?;
    let sites = heap.allocation_sites();
    if !sites.contains(site) {
        return None;
    }
    Some(match sites.decision(site) {
        PretenureDecision::Tenure => otter_gc::SitePlacement::Old,
        PretenureDecision::DontTenure => otter_gc::SitePlacement::Young(0),
        PretenureDecision::Undecided | PretenureDecision::MaybeTenure => {
            otter_gc::SitePlacement::Young(site)
        }
    })
}
fn concat(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    bits: [u64; 3],
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(vm) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let values = bits.map(Value::from_abi_bits);
    let Some(placement) = site_placement(&vm.gc_heap, values[2]) else {
        return NativeResultPair::miss();
    };
    let admitted = |value: Value| value.is_number() || value.as_string(&vm.gc_heap).is_some();
    if !admitted(values[0])
        || !admitted(values[1])
        || (values[0].is_number() && values[1].is_number())
    {
        return NativeResultPair::miss();
    }
    // SAFETY: exact current typed call packet and immutable safepoint remain
    // published until all scoped conversions and final cell initialization end.
    let Ok(roots) = (unsafe { alloc_value_stub_call_roots(ctx, safepoint, values) }) else {
        return NativeResultPair::miss();
    };
    let _roots = vm
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    if let Some(fast) = vm.try_concat_string_int32(roots.value(0), roots.value(1), placement) {
        return match fast {
            Ok(value) => NativeResultPair::success(value),
            Err(_) => NativeResultPair::out_of_memory(),
        };
    }
    vm.with_handle_scope(|vm, scope| {
        let lhs = vm.scoped_value(scope, roots.value(0));
        let rhs = vm.scoped_value(scope, roots.value(1));
        let left = vm.escape_scoped(lhs);
        let left = match left.as_string(&vm.gc_heap) {
            Some(string) => Ok(string),
            None => vm.js_string_for_concat(left, placement),
        };
        let left = match left {
            Ok(string) => vm.scoped_value(scope, Value::string(string)),
            Err(crate::VmError::OutOfMemory { .. }) => return NativeResultPair::out_of_memory(),
            Err(_) => return NativeResultPair::miss(),
        };
        let right = vm.escape_scoped(rhs);
        let right = match right.as_string(&vm.gc_heap) {
            Some(string) => Ok(string),
            None => vm.js_string_for_concat(right, placement),
        };
        let right = match right {
            Ok(string) => vm.scoped_value(scope, Value::string(string)),
            Err(crate::VmError::OutOfMemory { .. }) => return NativeResultPair::out_of_memory(),
            Err(_) => return NativeResultPair::miss(),
        };
        let left = vm
            .escape_scoped(left)
            .as_string(&vm.gc_heap)
            .expect("scoped converted string");
        let right = vm
            .escape_scoped(right)
            .as_string(&vm.gc_heap)
            .expect("scoped converted string");
        match crate::JsString::concat_at(left, right, &mut vm.gc_heap, placement) {
            Ok(value) => NativeResultPair::success(Value::string(value)),
            Err(crate::string::StringConcatError::StringTooLong { .. }) => NativeResultPair::miss(),
            Err(crate::string::StringConcatError::OutOfMemory(_)) => {
                NativeResultPair::out_of_memory()
            }
        }
    })
}

/// Return exact primitive ordering without managed allocation or user coercion.
#[must_use]
pub(crate) extern "C" fn primitive_string_order(
    heap: *const otter_gc::GcHeap,
    lhs: u64,
    rhs: u64,
) -> NativeResultPair {
    // SAFETY: the classified leaf ABI provides the current heap for this
    // synchronous immutable call. A null diagnostic fixture safely misses.
    let Some(heap) = (unsafe { heap.as_ref() }) else {
        return NativeResultPair::miss();
    };
    let lhs = Value::from_abi_bits(lhs);
    let rhs = Value::from_abi_bits(rhs);
    let order = if let (Some(a), Some(b)) = (lhs.as_string(heap), rhs.as_string(heap)) {
        Some(a.compare_lex(b, heap))
    } else {
        let numeric = |value: Value| {
            value.as_number().map(|number| number.as_f64()).or_else(|| {
                value.as_string(heap).map(|string| {
                    crate::number::parse::to_number_from_js_string(string, heap).as_f64()
                })
            })
        };
        let (Some(a), Some(b)) = (numeric(lhs), numeric(rhs)) else {
            return NativeResultPair::miss();
        };
        a.partial_cmp(&b)
    };
    let word = match order {
        Some(std::cmp::Ordering::Less) => -1,
        Some(std::cmp::Ordering::Equal) => 0,
        Some(std::cmp::Ordering::Greater) => 1,
        None => 2,
    };
    NativeResultPair::success(Value::number_i32(word))
}

#[cfg(test)]
mod tests;
