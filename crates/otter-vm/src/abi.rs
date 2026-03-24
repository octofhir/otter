//! Shared execution ABI definitions for the new VM.

use core::mem::size_of;

use crate::frame::FrameLayout;

/// Version tag for the execution ABI shared by the interpreter and the future JIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmAbiVersion {
    /// Initial ABI for the new `otter-vm` crate.
    V1,
}

/// Number of bytes in one register value cell shared by the interpreter and JIT.
pub const REGISTER_VALUE_SIZE_BYTES: usize = size_of::<crate::value::RegisterValue>();
/// Number of bytes in one register index shared by frame metadata and JIT codegen.
pub const REGISTER_INDEX_SIZE_BYTES: usize = size_of::<crate::frame::RegisterIndex>();

/// Value-level ABI requirements shared by all execution tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueAbi {
    register_value_size_bytes: usize,
    register_index_size_bytes: usize,
    nan_boxed_values: bool,
}

impl ValueAbi {
    /// Returns the current value ABI used by `otter-vm`.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            register_value_size_bytes: REGISTER_VALUE_SIZE_BYTES,
            register_index_size_bytes: REGISTER_INDEX_SIZE_BYTES,
            nan_boxed_values: true,
        }
    }

    /// Returns the size of one register value cell in bytes.
    #[must_use]
    pub const fn register_value_size_bytes(self) -> usize {
        self.register_value_size_bytes
    }

    /// Returns the size of one register index in bytes.
    #[must_use]
    pub const fn register_index_size_bytes(self) -> usize {
        self.register_index_size_bytes
    }

    /// Returns whether register values use the shared NaN-boxed layout.
    #[must_use]
    pub const fn nan_boxed_values(self) -> bool {
        self.nan_boxed_values
    }
}

/// Frame-level ABI requirements shared by interpreter and JIT code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameAbiRequirements {
    layout: FrameLayout,
    flat_register_file: bool,
    contiguous_argument_window: bool,
    user_visible_registers_contiguous: bool,
    receiver_in_hidden_slot: bool,
}

impl FrameAbiRequirements {
    /// Builds the shared frame ABI requirements for one function layout.
    #[must_use]
    pub const fn new(layout: FrameLayout) -> Self {
        Self {
            layout,
            flat_register_file: true,
            contiguous_argument_window: true,
            user_visible_registers_contiguous: true,
            receiver_in_hidden_slot: layout.receiver_slot().is_some(),
        }
    }

    /// Returns the shared frame layout.
    #[must_use]
    pub const fn layout(self) -> FrameLayout {
        self.layout
    }

    /// Returns whether all execution tiers use one flat register file.
    #[must_use]
    pub const fn flat_register_file(self) -> bool {
        self.flat_register_file
    }

    /// Returns whether call arguments use one contiguous window.
    #[must_use]
    pub const fn contiguous_argument_window(self) -> bool {
        self.contiguous_argument_window
    }

    /// Returns whether bytecode-visible registers stay contiguous.
    #[must_use]
    pub const fn user_visible_registers_contiguous(self) -> bool {
        self.user_visible_registers_contiguous
    }

    /// Returns whether the frame reserves a hidden receiver / `this` slot.
    #[must_use]
    pub const fn receiver_in_hidden_slot(self) -> bool {
        self.receiver_in_hidden_slot
    }
}

/// Runtime-level ABI requirements shared by interpreter and JIT entrypoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeAbiRequirements {
    version: VmAbiVersion,
    value_abi: ValueAbi,
    shared_frame_model: bool,
    shared_calling_convention: bool,
}

impl RuntimeAbiRequirements {
    /// Returns the current runtime ABI contract.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            version: VmAbiVersion::V1,
            value_abi: ValueAbi::current(),
            shared_frame_model: true,
            shared_calling_convention: true,
        }
    }

    /// Returns the ABI version.
    #[must_use]
    pub const fn version(self) -> VmAbiVersion {
        self.version
    }

    /// Returns the shared value ABI.
    #[must_use]
    pub const fn value_abi(self) -> ValueAbi {
        self.value_abi
    }

    /// Returns whether interpreter and JIT share one frame model.
    #[must_use]
    pub const fn shared_frame_model(self) -> bool {
        self.shared_frame_model
    }

    /// Returns whether interpreter and JIT share one calling convention.
    #[must_use]
    pub const fn shared_calling_convention(self) -> bool {
        self.shared_calling_convention
    }
}

#[cfg(test)]
mod tests {
    use crate::frame::FrameLayout;

    use super::{
        FrameAbiRequirements, REGISTER_INDEX_SIZE_BYTES, REGISTER_VALUE_SIZE_BYTES,
        RuntimeAbiRequirements, ValueAbi, VmAbiVersion,
    };

    #[test]
    fn value_abi_matches_shared_register_representation() {
        let abi = ValueAbi::current();

        assert_eq!(abi.register_value_size_bytes(), REGISTER_VALUE_SIZE_BYTES);
        assert_eq!(abi.register_index_size_bytes(), REGISTER_INDEX_SIZE_BYTES);
        assert!(abi.nan_boxed_values());
    }

    #[test]
    fn frame_abi_requires_flat_contiguous_execution_model() {
        let layout = FrameLayout::new(1, 2, 3, 4).expect("layout should be valid");
        let abi = FrameAbiRequirements::new(layout);

        assert_eq!(abi.layout(), layout);
        assert!(abi.flat_register_file());
        assert!(abi.contiguous_argument_window());
        assert!(abi.user_visible_registers_contiguous());
        assert!(abi.receiver_in_hidden_slot());
    }

    #[test]
    fn runtime_abi_stays_on_v1_contract() {
        let abi = RuntimeAbiRequirements::current();

        assert_eq!(abi.version(), VmAbiVersion::V1);
        assert!(abi.shared_frame_model());
        assert!(abi.shared_calling_convention());
    }
}
