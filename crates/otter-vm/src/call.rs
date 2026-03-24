//! Call-site side tables for the new VM.

use crate::bytecode::BytecodeRegister;
use crate::bytecode::ProgramCounter;
use crate::frame::{FrameFlags, RegisterIndex};
use crate::module::FunctionIndex;

/// Direct-call metadata attached to a bytecode call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectCall {
    callee: FunctionIndex,
    argument_count: RegisterIndex,
    flags: FrameFlags,
    receiver: Option<BytecodeRegister>,
}

impl DirectCall {
    /// Creates direct-call metadata for one call site.
    #[must_use]
    pub const fn new(
        callee: FunctionIndex,
        argument_count: RegisterIndex,
        flags: FrameFlags,
    ) -> Self {
        Self {
            callee,
            argument_count,
            flags,
            receiver: None,
        }
    }

    /// Creates direct-call metadata with an explicit receiver register.
    #[must_use]
    pub const fn new_with_receiver(
        callee: FunctionIndex,
        argument_count: RegisterIndex,
        flags: FrameFlags,
        receiver: BytecodeRegister,
    ) -> Self {
        Self {
            callee,
            argument_count,
            flags,
            receiver: Some(receiver),
        }
    }

    /// Returns the callee function index.
    #[must_use]
    pub const fn callee(self) -> FunctionIndex {
        self.callee
    }

    /// Returns the actual argument count for the call site.
    #[must_use]
    pub const fn argument_count(self) -> RegisterIndex {
        self.argument_count
    }

    /// Returns the per-call frame flags.
    #[must_use]
    pub const fn flags(self) -> FrameFlags {
        self.flags
    }

    /// Returns the caller-visible receiver register, if one is attached.
    #[must_use]
    pub const fn receiver(self) -> Option<BytecodeRegister> {
        self.receiver
    }
}

/// Closure-call metadata attached to a bytecode call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClosureCall {
    argument_count: RegisterIndex,
    flags: FrameFlags,
    receiver: Option<BytecodeRegister>,
}

impl ClosureCall {
    /// Creates closure-call metadata for one call site.
    #[must_use]
    pub const fn new(argument_count: RegisterIndex, flags: FrameFlags) -> Self {
        Self {
            argument_count,
            flags,
            receiver: None,
        }
    }

    /// Creates closure-call metadata with an explicit receiver register.
    #[must_use]
    pub const fn new_with_receiver(
        argument_count: RegisterIndex,
        flags: FrameFlags,
        receiver: BytecodeRegister,
    ) -> Self {
        Self {
            argument_count,
            flags,
            receiver: Some(receiver),
        }
    }

    /// Returns the actual argument count for the call site.
    #[must_use]
    pub const fn argument_count(self) -> RegisterIndex {
        self.argument_count
    }

    /// Returns the per-call frame flags.
    #[must_use]
    pub const fn flags(self) -> FrameFlags {
        self.flags
    }

    /// Returns the caller-visible receiver register, if one is attached.
    #[must_use]
    pub const fn receiver(self) -> Option<BytecodeRegister> {
        self.receiver
    }
}

/// Call-site metadata for the current VM subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallSite {
    /// Direct call with a statically known callee.
    Direct(DirectCall),
    /// Call through a closure object stored in a register.
    Closure(ClosureCall),
}

/// Immutable call-site table for one function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTable {
    sites: Box<[Option<CallSite>]>,
}

impl CallTable {
    /// Creates a call-site table from owned entries indexed by bytecode PC.
    #[must_use]
    pub fn new(sites: Vec<Option<CallSite>>) -> Self {
        Self {
            sites: sites.into_boxed_slice(),
        }
    }

    /// Creates an empty call-site table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of stored PC slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Returns `true` when the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// Returns the call-site metadata for the given bytecode PC.
    #[must_use]
    pub fn get(&self, pc: ProgramCounter) -> Option<CallSite> {
        self.sites.get(pc as usize).copied().flatten()
    }

    /// Returns the direct-call metadata for the given bytecode PC.
    #[must_use]
    pub fn get_direct(&self, pc: ProgramCounter) -> Option<DirectCall> {
        match self.get(pc) {
            Some(CallSite::Direct(call)) => Some(call),
            _ => None,
        }
    }

    /// Returns the closure-call metadata for the given bytecode PC.
    #[must_use]
    pub fn get_closure(&self, pc: ProgramCounter) -> Option<ClosureCall> {
        match self.get(pc) {
            Some(CallSite::Closure(call)) => Some(call),
            _ => None,
        }
    }
}

impl Default for CallTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::BytecodeRegister;
    use crate::frame::FrameFlags;

    use super::{CallSite, CallTable, ClosureCall, DirectCall};

    #[test]
    fn call_table_resolves_call_sites_by_pc() {
        let direct_call = DirectCall::new(crate::module::FunctionIndex(3), 2, FrameFlags::empty());
        let closure_call = ClosureCall::new(1, FrameFlags::empty());
        let table = CallTable::new(vec![
            None,
            Some(CallSite::Direct(direct_call)),
            Some(CallSite::Closure(closure_call)),
        ]);

        assert_eq!(table.len(), 3);
        assert_eq!(table.get(0), None);
        assert_eq!(table.get_direct(1), Some(direct_call));
        assert_eq!(table.get_closure(2), Some(closure_call));
        assert_eq!(table.get(3), None);
    }

    #[test]
    fn call_metadata_keeps_receiver_registers() {
        let direct_call = DirectCall::new_with_receiver(
            crate::module::FunctionIndex(3),
            2,
            FrameFlags::new(false, true, false),
            BytecodeRegister::new(7),
        );
        let closure_call = ClosureCall::new_with_receiver(
            1,
            FrameFlags::new(false, true, false),
            BytecodeRegister::new(4),
        );

        assert_eq!(direct_call.receiver(), Some(BytecodeRegister::new(7)));
        assert_eq!(closure_call.receiver(), Some(BytecodeRegister::new(4)));
        assert!(direct_call.flags().has_receiver());
        assert!(closure_call.flags().has_receiver());
    }
}
