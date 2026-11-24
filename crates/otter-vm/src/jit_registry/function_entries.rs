//! Stable function-entry cells and their shared dense address directory.
//!
//! # Contents
//! - `FunctionEntries` owns one cell per linked function in fixed-size pages.
//! - `EntryDirectory` publishes the addresses to generated code and workers.
//!
//! # Invariants
//! - A published cell never moves, including across directory growth or the
//!   first compilation of a lazy body. Cells live until the registry drops.
//! - Page allocation and address publication run on the isolate mutator.
//! - Generated code reads the current slice from its CodeRegistryView. A
//!   compiler worker gets one copied address under the directory read guard.
//!
//! # See also
//! - `crate::native_abi::FunctionEntryCell` defines the private call ABI.

use std::sync::Arc;

use crate::code_space::FunctionHeader;
use crate::mutator_owned::MutatorCell;
use crate::native_abi::{CodeRegistryView, FunctionEntryCell};

/// One vector of addresses shared by generated entry selection and compilation.
/// On the mutator the same vector backs `CodeRegistryView::function_entries`.
pub(crate) type EntryDirectory = MutatorCell<Vec<u64>>;

const PAGE_SIZE: usize = 128;
type Page = Box<[Option<FunctionEntryCell>]>;

#[derive(Default)]
pub(super) struct FunctionEntries {
    pages: Vec<Option<Page>>,
    addresses: Arc<EntryDirectory>,
}

impl FunctionEntries {
    pub(super) fn directory(&self) -> Arc<EntryDirectory> {
        Arc::clone(&self.addresses)
    }

    pub(super) fn get(&self, id: u32) -> Option<&FunctionEntryCell> {
        let index = id as usize;
        self.pages.get(index / PAGE_SIZE)?.as_ref()?[index % PAGE_SIZE].as_ref()
    }

    pub(super) fn get_mut(&mut self, id: u32) -> Option<&mut FunctionEntryCell> {
        let index = id as usize;
        self.pages.get_mut(index / PAGE_SIZE)?.as_mut()?[index % PAGE_SIZE].as_mut()
    }

    pub(super) fn link(
        &mut self,
        headers: impl IntoIterator<Item = (FunctionHeader, u32)>,
        view: &mut CodeRegistryView,
    ) {
        let mut addresses = self.addresses.borrow_mut();
        for (header, realm) in headers {
            let id = header.id as usize;
            if self.pages.len() <= id / PAGE_SIZE {
                self.pages.resize_with(id / PAGE_SIZE + 1, || None);
            }
            let page = self.pages[id / PAGE_SIZE]
                .get_or_insert_with(|| (0..PAGE_SIZE).map(|_| None).collect());
            let cell = page[id % PAGE_SIZE].get_or_insert_with(|| {
                FunctionEntryCell::unpublished(
                    header.id,
                    header.param_count,
                    header.register_count,
                    header.call_flags,
                    realm,
                )
            });
            assert_layout(cell, header.param_count, header.register_count);
            if addresses.len() <= id {
                addresses.resize(id + 1, 0);
            }
            if addresses[id] == 0 {
                cell.restore_interpreter();
                addresses[id] = std::ptr::from_ref(cell) as u64;
            }
        }
        view.function_entries = addresses.as_ptr() as u64;
        view.function_entry_count = addresses.len() as u64;
    }
}

pub(super) fn assert_layout(cell: &FunctionEntryCell, params: u16, registers: u16) {
    assert_eq!(
        cell.param_count, params,
        "linked function parameter layout is immutable"
    );
    assert_eq!(
        cell.register_count, registers,
        "linked function register layout is immutable"
    );
}
