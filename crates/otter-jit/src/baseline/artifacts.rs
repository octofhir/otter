//! Stable data owned by one baseline compilation result.
//!
//! # Contents
//! - Self-patching property IC cells whose addresses are embedded in code.
//! - Plan-owned variadic operand buffers passed to runtime stubs.
//! - Emission cursors checked against the backend-neutral lowering plan.
//!
//! # Invariants
//! - Every returned pointer targets boxed storage that survives for the entire
//!   lifetime of the compiled code object.
//! - Property IC cursors consume exactly the counts established by lowering.
//! - Storage is moved only by owner; boxed payload addresses never change.
//!
//! # See also
//! - [`super::lowering::BaselinePlan`] computes the required IC capacities.
//! - [`super::BaselineCode`] owns the finalized storage.

use super::WhiskerIcCell;

pub(crate) struct EmissionArtifacts {
    pub(crate) load_ic_cells: Box<[WhiskerIcCell]>,
    pub(crate) store_ic_cells: Box<[WhiskerIcCell]>,
    pub(crate) register_operands: Box<[u16]>,
    pub(crate) index_operands: Box<[u32]>,
    next_load_ic: usize,
    next_store_ic: usize,
}

impl EmissionArtifacts {
    pub(crate) fn new(load_property_count: usize, store_property_count: usize) -> Self {
        Self {
            load_ic_cells: vec![WhiskerIcCell::default(); load_property_count].into_boxed_slice(),
            store_ic_cells: vec![WhiskerIcCell::default(); store_property_count].into_boxed_slice(),
            register_operands: Box::default(),
            index_operands: Box::default(),
            next_load_ic: 0,
            next_store_ic: 0,
        }
    }

    pub(crate) fn next_load_ic_addr(&mut self) -> usize {
        let cell = self
            .load_ic_cells
            .get_mut(self.next_load_ic)
            .expect("lowering undercounted LoadProperty IC cells");
        self.next_load_ic += 1;
        cell as *mut WhiskerIcCell as usize
    }

    pub(crate) fn next_store_ic_addr(&mut self) -> usize {
        let cell = self
            .store_ic_cells
            .get_mut(self.next_store_ic)
            .expect("lowering undercounted StoreProperty IC cells");
        self.next_store_ic += 1;
        cell as *mut WhiskerIcCell as usize
    }

    pub(crate) fn retain_operand_buffers(&mut self, registers: Box<[u16]>, indices: Box<[u32]>) {
        assert!(self.register_operands.is_empty());
        assert!(self.index_operands.is_empty());
        self.register_operands = registers;
        self.index_operands = indices;
    }

    pub(crate) fn finish(self) -> Self {
        assert_eq!(
            self.next_load_ic,
            self.load_ic_cells.len(),
            "lowering overcounted LoadProperty IC cells"
        );
        assert_eq!(
            self.next_store_ic,
            self.store_ic_cells.len(),
            "lowering overcounted StoreProperty IC cells"
        );
        self
    }
}

#[cfg(test)]
mod tests {
    use super::EmissionArtifacts;

    #[test]
    fn retained_table_and_ic_addresses_survive_finish() {
        let mut artifacts = EmissionArtifacts::new(1, 1);
        let load = artifacts.next_load_ic_addr();
        let store = artifacts.next_store_ic_addr();
        let regs = vec![2, 4, 6].into_boxed_slice();
        let regs_ptr = regs.as_ptr();
        artifacts.retain_operand_buffers(regs, vec![1].into_boxed_slice());
        let artifacts = artifacts.finish();

        assert_eq!(load, artifacts.load_ic_cells.as_ptr() as usize);
        assert_eq!(store, artifacts.store_ic_cells.as_ptr() as usize);
        assert_eq!(regs_ptr, artifacts.register_operands.as_ptr());
        assert_eq!(&*artifacts.register_operands, &[2, 4, 6]);
        assert_eq!(&*artifacts.index_operands, &[1]);
    }

    #[test]
    #[should_panic(expected = "overcounted LoadProperty")]
    fn finish_rejects_unconsumed_ic_capacity() {
        EmissionArtifacts::new(1, 0).finish();
    }
}
