//! Physical ownership of immutable compiler and executable wordcode arrays.
//!
//! # Contents
//! - One shared allocation owns both arrays and their inseparable physical lease.
//! - Admission preserves all instruction/operand bytes and existing verifier proofs.
//!
//! # Invariants
//! - Mutable compiler/linker builders never mutate this storage.
//! - Admitted clones share both bytes and charge; the final drop releases both.
//! - Unadmitted aliases detach fallibly before executable publication.
//! - Requested arrays and any extra Vec capacity are admitted before publication.
//! - No isolate identity, feedback, GC handle or runtime cache belongs here.
//!
//! # See also
//! - `super::FunctionCode` exposes the immutable body and independent builders.
//! - `otter_resource::ResourceLease` owns exact physical admission.

use super::Instruction;
use otter_resource::{ResourceAccount, ResourceClass, ResourceError, ResourceLease};
use std::sync::Arc;

#[derive(Debug, Default)]
pub(super) struct Storage {
    pub(super) instructions: Box<[Instruction]>,
    pub(super) overflow_operand_words: Box<[u32]>,
    lease: Option<ResourceLease>,
}

impl Storage {
    pub(super) fn new(
        instructions: Box<[Instruction]>,
        overflow_operand_words: Box<[u32]>,
    ) -> Self {
        Self {
            instructions,
            overflow_operand_words,
            lease: None,
        }
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        (std::mem::size_of::<Self>() as u64)
            .saturating_add((2 * std::mem::size_of::<usize>()) as u64)
            .saturating_add(std::mem::size_of_val(&*self.instructions) as u64)
            .saturating_add(std::mem::size_of_val(&*self.overflow_operand_words) as u64)
    }

    pub(super) fn admit(
        owner: &mut Arc<Self>,
        account: &ResourceAccount,
    ) -> Result<(), ResourceError> {
        if owner.lease.is_some() {
            return Ok(());
        }
        let mut lease =
            account.reserve_exact(ResourceClass::SourceModuleBytes, owner.retained_bytes())?;
        let storage = if Arc::get_mut(owner).is_some() {
            Arc::get_mut(owner).expect("unique wordcode owner")
        } else {
            let instructions = copy_words(&owner.instructions, &mut lease)?;
            let overflow = copy_words(&owner.overflow_operand_words, &mut lease)?;
            let detached = Arc::new(Self::new(instructions, overflow));
            lease.resize(detached.retained_bytes())?;
            *owner = detached;
            Arc::get_mut(owner).expect("detached wordcode owner")
        };
        storage.lease = Some(lease);
        Ok(())
    }
}

// This is the only array copy in the ownership boundary: an externally retained,
// unadmitted compiler alias cannot acquire an escaped VM lease on refusal.
fn copy_words<T: Copy>(source: &[T], lease: &mut ResourceLease) -> Result<Box<[T]>, ResourceError> {
    let bytes = (source.len() as u64).saturating_mul(std::mem::size_of::<T>() as u64);
    let mut words = Vec::new();
    words
        .try_reserve_exact(source.len())
        .map_err(|cause| ResourceError::Allocation {
            class: ResourceClass::SourceModuleBytes,
            capacity: source.len(),
            requested: bytes,
            cause,
        })?;
    let extra = (words.capacity().saturating_sub(source.len()) as u64)
        .saturating_mul(std::mem::size_of::<T>() as u64);
    if extra != 0 {
        lease.resize(lease.amount().saturating_add(extra))?;
    }
    words.extend_from_slice(source);
    Ok(words.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionCodeBuilder, Op, Operand};

    #[test]
    fn shared_wordcode_keeps_one_physical_charge_until_the_last_owner() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(7)]);
        builder.push(Op::ReturnValue, &[Operand::Register(0)]);
        let account = ResourceAccount::default();
        let mut code = builder.finish();
        code.admit_storage(&account).unwrap();
        let bytes = code.retained_bytes();
        let mut shared = code.clone();
        assert!(code.shares_storage(&shared));
        assert!(std::ptr::eq(code.get(0).unwrap(), shared.get(0).unwrap()));
        let other_account = ResourceAccount::default();
        shared.admit_storage(&other_account).unwrap();
        assert_eq!(
            other_account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            bytes
        );
        let mut edited = shared.to_builder();
        assert!(edited.set_operand(0, 1, Operand::Imm32(9)));
        let mut edited = edited.finish();
        let raw_compiler_alias = edited.clone();
        let denied = ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(ResourceClass::SourceModuleBytes, 0)
                .build(),
        );
        assert!(edited.admit_storage(&denied).is_err());
        assert!(edited.shares_storage(&raw_compiler_alias));
        assert_eq!(
            denied
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
        edited.admit_storage(&other_account).unwrap();
        assert!(!edited.shares_storage(&raw_compiler_alias));
        assert_eq!(
            other_account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            edited.retained_bytes()
        );
        assert!(!edited.shares_storage(&shared));
        assert_eq!(
            shared.operand(shared.get(0).unwrap(), 1),
            Some(Operand::Imm32(7))
        );
        drop(edited);
        assert_eq!(
            other_account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
        assert_eq!(
            raw_compiler_alias.operand(raw_compiler_alias.get(0).unwrap(), 1),
            Some(Operand::Imm32(9))
        );
        drop(code);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            bytes
        );
        drop(shared);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
    }
}
