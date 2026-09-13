//! Decode inline activation recipes from the current precise Machine roots.
//!
//! # Contents
//! - Generation/safepoint validation and boxed instances of VM-owned frames.
//!
//! # Invariants
//! - Code-owned recipes contain root indices, never moving addresses or values.
//! - Every read belongs to the current published code generation and safepoint.
//! - Decoding cannot collect or reenter; the caller publishes the resulting
//!   frames before executing the committed property operation.

use super::{JitCtx, WhiskerIcCell};
use otter_vm::{
    Value, VmError,
    deopt::{DeoptFrame, DeoptFrameEntry},
};

impl WhiskerIcCell {
    pub(super) fn decode_inline_frames(
        &self,
        ctx: &JitCtx,
    ) -> Result<Box<[DeoptFrame<Value>]>, VmError> {
        if self.inline_frames.is_empty() {
            return Ok(Box::default());
        }
        // SAFETY: the JIT context retains the VM's root-chain head. The current
        // generated cold call has published its record until this stub returns.
        let address = unsafe { ctx.machine_roots_ptr.as_ref() }
            .copied()
            .ok_or(VmError::InvalidOperand)?;
        let roots = unsafe { (address as *const otter_vm::jit::JitMachineRootRecord).as_ref() }
            .ok_or(VmError::InvalidOperand)?;
        self.decode_published_frames(roots)
    }

    fn decode_published_frames(
        &self,
        roots: &otter_vm::jit::JitMachineRootRecord,
    ) -> Result<Box<[DeoptFrame<Value>]>, VmError> {
        if self.inline_owner != Some((roots.code_object_id, roots.safepoint_id))
            || (roots.root_count != 0 && roots.root_base.is_null())
        {
            return Err(VmError::InvalidOperand);
        }
        let slot = |slot: &Option<u16>| -> Result<Value, VmError> {
            match slot {
                None => Ok(Value::undefined()),
                Some(index) if *index < roots.root_count => {
                    // SAFETY: bounds checked against the current live root window.
                    Ok(Value::from_bits(unsafe {
                        *roots.root_base.add(usize::from(*index))
                    }))
                }
                _ => Err(VmError::InvalidOperand),
            }
        };
        self.inline_frames
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_recipes_require_the_current_generation_and_root_window() {
        let mut slots = [
            Value::function(7).to_bits(),
            Value::number_f64(3.25).to_bits(),
        ];
        let mut record = otter_vm::jit::JitMachineRootRecord {
            previous: 0,
            root_base: slots.as_mut_ptr(),
            code_object_id: 9,
            root_count: 2,
            reserved: 0,
            safepoint_id: 3,
        };
        let mut cell = WhiskerIcCell::default();
        cell.set_inline_frames(
            9,
            3,
            Box::new([DeoptFrame {
                function_id: 7,
                byte_pc: 24,
                entry: Some(DeoptFrameEntry {
                    return_register: 1,
                    this: Some(1),
                    closure: Some(0),
                }),
                slots: Box::new([None, Some(1)]),
            }]),
        );
        let frames = cell.decode_published_frames(&record).unwrap();
        assert_eq!(frames[0].slots[0], Value::undefined());
        assert_eq!(frames[0].slots[1], Value::number_f64(3.25));
        assert_eq!(
            frames[0].entry.as_ref().unwrap().closure,
            Value::function(7)
        );
        assert_eq!(
            frames[0].entry.as_ref().unwrap().this,
            Value::number_f64(3.25)
        );
        // SAFETY: the fixture keeps its two initialized root slots live.
        unsafe {
            record
                .root_base
                .add(1)
                .write(Value::number_f64(9.5).to_bits());
        }
        assert_eq!(
            cell.decode_published_frames(&record).unwrap()[0].slots[1],
            Value::number_f64(9.5)
        );
        record.code_object_id = 10;
        assert!(cell.decode_published_frames(&record).is_err());
        record.code_object_id = 9;
        record.safepoint_id = 4;
        assert!(cell.decode_published_frames(&record).is_err());
        record.safepoint_id = 3;
        record.root_count = 1;
        assert!(cell.decode_published_frames(&record).is_err());
        record.root_count = 2;
        record.root_base = std::ptr::null_mut();
        assert!(cell.decode_published_frames(&record).is_err());
    }
}
