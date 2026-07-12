//! Stable data owned by one baseline compilation result.
//!
//! # Contents
//! - Self-patching property IC cells whose addresses are embedded in code.
//! - Decoded variadic operand tables passed to runtime stubs.
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
    pub(crate) array_literal_regs: Vec<Box<[u16]>>,
    pub(crate) closure_parent_indices: Vec<Box<[u32]>>,
    pub(crate) math_argument_regs: Vec<Box<[u16]>>,
    next_load_ic: usize,
    next_store_ic: usize,
}

impl EmissionArtifacts {
    pub(crate) fn new(load_property_count: usize, store_property_count: usize) -> Self {
        Self {
            load_ic_cells: vec![WhiskerIcCell::default(); load_property_count].into_boxed_slice(),
            store_ic_cells: vec![WhiskerIcCell::default(); store_property_count].into_boxed_slice(),
            array_literal_regs: Vec::new(),
            closure_parent_indices: Vec::new(),
            math_argument_regs: Vec::new(),
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

    pub(crate) fn retain_array_literal_regs(&mut self, regs: Box<[u16]>) -> *const u16 {
        let ptr = regs.as_ptr();
        self.array_literal_regs.push(regs);
        ptr
    }

    pub(crate) fn retain_closure_parent_indices(&mut self, indices: Box<[u32]>) -> *const u32 {
        let ptr = indices.as_ptr();
        self.closure_parent_indices.push(indices);
        ptr
    }

    pub(crate) fn retain_math_argument_regs(&mut self, regs: Box<[u16]>) -> *const u16 {
        let ptr = regs.as_ptr();
        self.math_argument_regs.push(regs);
        ptr
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
        let regs = artifacts.retain_array_literal_regs(vec![2, 4, 6].into_boxed_slice());
        let artifacts = artifacts.finish();

        assert_eq!(load, artifacts.load_ic_cells.as_ptr() as usize);
        assert_eq!(store, artifacts.store_ic_cells.as_ptr() as usize);
        assert_eq!(regs, artifacts.array_literal_regs[0].as_ptr());
        assert_eq!(&*artifacts.array_literal_regs[0], &[2, 4, 6]);
    }

    #[test]
    #[should_panic(expected = "overcounted LoadProperty")]
    fn finish_rejects_unconsumed_ic_capacity() {
        EmissionArtifacts::new(1, 0).finish();
    }
}
