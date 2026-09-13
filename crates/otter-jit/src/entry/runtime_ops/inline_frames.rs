//! Decode inline activation recipes from code-owned safepoint metadata.
//!
//! # Contents
//! - Exact active-generation lookup through the published code registry.
//! - Boxed descendants reconstructed from the current precise spill window.
//!
//! # Invariants
//! - A suspended outer Machine root record never supplies a callee's recipe.
//! - Recipes contain indices only; decoding cannot collect or reenter.
//! - No registry/recipe borrow survives the operation that publishes the frames.

use super::JitCtx;
use otter_vm::{
    Value, VmError,
    deopt::{DeoptFrame, DeoptFrameEntry},
    jit::JitMachineRootRecord,
    native_abi::{CodeRegistryView, SafepointRecord, TaggedLocation},
};

pub(super) fn decode(ctx: &JitCtx) -> Result<Box<[DeoptFrame<Value>]>, VmError> {
    // SAFETY: JitCtx retains the published thread, root-chain head and current
    // stack record throughout this non-reentrant decoder. Null fixture/Template
    // entries have no inline reentry metadata.
    let Some(thread) = (unsafe { ctx.thread.as_ref() }) else {
        return Ok(Box::default());
    };
    let address = unsafe { ctx.machine_roots_ptr.as_ref() }
        .copied()
        .unwrap_or(0);
    let Some(roots) = (unsafe { (address as *const JitMachineRootRecord).as_ref() }) else {
        return Ok(Box::default());
    };
    if roots.code_object_id != thread.current_code_object_id {
        return Ok(Box::default());
    }
    // SAFETY: the VM publishes a stable registry view for every active code object.
    let registry = unsafe { (thread.code_registry as *const CodeRegistryView).as_ref() }
        .ok_or(VmError::InvalidOperand)?;
    decode_from_registry(registry, roots)
}

fn decode_from_registry(
    registry: &CodeRegistryView,
    roots: &JitMachineRootRecord,
) -> Result<Box<[DeoptFrame<Value>]>, VmError> {
    // SAFETY: root publication and registry ownership span this cold call.
    let record = unsafe { registry.resolve(roots.code_object_id, roots.safepoint_id) }
        .ok_or(VmError::InvalidOperand)?;
    // SAFETY: the resolved immutable record is retained by the live generation.
    let record = unsafe { &*record };
    if record.id != roots.safepoint_id {
        return Err(VmError::InvalidOperand);
    }
    decode_published_frames(record, roots)
}

fn decode_published_frames(
    record: &SafepointRecord,
    roots: &JitMachineRootRecord,
) -> Result<Box<[DeoptFrame<Value>]>, VmError> {
    if record.inline_frames.is_empty() {
        return Ok(Box::default());
    }
    if roots.root_count != 0 && roots.root_base.is_null() {
        return Err(VmError::InvalidOperand);
    }
    let slot = |slot: &Option<u16>| -> Result<Value, VmError> {
        match slot {
            None => Ok(Value::undefined()),
            Some(index)
                if *index < roots.root_count
                    && record
                        .tagged_locations
                        .contains(&TaggedLocation::spill_slot(*index)) =>
            {
                // SAFETY: the exact record proves this initialized live spill root.
                Ok(Value::from_bits(unsafe {
                    *roots.root_base.add(usize::from(*index))
                }))
            }
            _ => Err(VmError::InvalidOperand),
        }
    };
    record
        .inline_frames
        .iter()
        .map(|frame| {
            let entry = frame.entry.as_ref().ok_or(VmError::InvalidOperand)?;
            Ok(DeoptFrame {
                function_id: frame.function_id,
                byte_pc: frame.byte_pc,
                entry: Some(DeoptFrameEntry {
                    return_register: entry.return_register,
                    this: slot(&entry.this)?,
                    closure: slot(&entry.closure)?,
                }),
                slots: frame.slots.iter().map(&slot).collect::<Result<_, _>>()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn resolve(context: u64, code: u64, id: u32) -> *const SafepointRecord {
        // SAFETY: fixture publishes its stack record for the resolver call.
        let record = unsafe { &*(context as *const SafepointRecord) };
        if code == 9 && id == record.id {
            record
        } else {
            std::ptr::null()
        }
    }

    #[test]
    fn inline_recipes_require_exact_registry_generation_and_live_roots() {
        let mut slots = [
            Value::function(7).to_bits(),
            Value::number_f64(3.25).to_bits(),
        ];
        let mut roots = JitMachineRootRecord {
            previous: 0,
            root_base: slots.as_mut_ptr(),
            code_object_id: 9,
            root_count: 2,
            reserved: 0,
            safepoint_id: 3,
        };
        let mut record = SafepointRecord {
            id: 3,
            frame_state: otter_vm::native_abi::NO_FRAME_STATE,
            tagged_locations: vec![TaggedLocation::spill_slot(0), TaggedLocation::spill_slot(1)],
            inline_frames: Box::new([DeoptFrame {
                function_id: 7,
                byte_pc: 24,
                entry: Some(DeoptFrameEntry {
                    return_register: 1,
                    this: Some(1),
                    closure: Some(0),
                }),
                slots: Box::new([None, Some(1)]),
            }]),
        };
        let registry = CodeRegistryView {
            context: std::ptr::from_ref(&record) as u64,
            resolve_safepoint: resolve as *const () as u64,
            hot_function: 0,
        };
        let frames = decode_from_registry(&registry, &roots).unwrap();
        assert_eq!(frames[0].slots[0], Value::undefined());
        assert_eq!(frames[0].slots[1], Value::number_f64(3.25));
        assert_eq!(
            frames[0].entry.as_ref().unwrap().closure,
            Value::function(7)
        );
        // SAFETY: the initialized fixture spill window stays live.
        unsafe {
            roots
                .root_base
                .add(1)
                .write(Value::number_f64(9.5).to_bits());
        }
        assert_eq!(
            decode_from_registry(&registry, &roots).unwrap()[0]
                .entry
                .as_ref()
                .unwrap()
                .this,
            Value::number_f64(9.5)
        );
        roots.code_object_id = 10;
        assert!(decode_from_registry(&registry, &roots).is_err());
        roots.code_object_id = 9;
        roots.safepoint_id = 4;
        assert!(decode_from_registry(&registry, &roots).is_err());
        roots.safepoint_id = 3;
        roots.root_count = 1;
        assert!(decode_from_registry(&registry, &roots).is_err());
        roots.root_count = 2;
        record.tagged_locations.pop();
        assert!(decode_from_registry(&registry, &roots).is_err());
        record.tagged_locations.push(TaggedLocation::spill_slot(1));
        roots.root_base = std::ptr::null_mut();
        assert!(decode_from_registry(&registry, &roots).is_err());
    }
}
