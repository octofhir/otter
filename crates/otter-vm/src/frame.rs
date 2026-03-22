//! Frame layout and activation metadata for the new VM.
//!
//! The new VM uses a single register file per frame.
//! Hidden slots are stored in a reserved prefix of the frame window and are not
//! addressable by bytecode. User-visible registers follow immediately after:
//!
//! 1. hidden slots
//! 2. parameter slots
//! 3. local slots
//! 4. temporary slots

use core::fmt;

/// Register index inside a frame window.
pub type RegisterIndex = u16;

/// Errors produced while constructing a frame layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameLayoutError {
    /// The total register count would exceed the register index space.
    RegisterCountOverflow,
}

impl fmt::Display for FrameLayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegisterCountOverflow => {
                f.write_str("frame register count exceeds the supported index space")
            }
        }
    }
}

impl std::error::Error for FrameLayoutError {}

/// Contiguous register range within a frame window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegisterRange {
    start: RegisterIndex,
    len: RegisterIndex,
}

impl RegisterRange {
    /// Creates a register range.
    #[must_use]
    pub const fn new(start: RegisterIndex, len: RegisterIndex) -> Self {
        Self { start, len }
    }

    /// Returns the start register index of the range.
    #[must_use]
    pub const fn start(self) -> RegisterIndex {
        self.start
    }

    /// Returns the number of registers in the range.
    #[must_use]
    pub const fn len(self) -> RegisterIndex {
        self.len
    }

    /// Returns `true` when the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Returns the exclusive end register index of the range.
    #[must_use]
    pub const fn end(self) -> RegisterIndex {
        self.start.saturating_add(self.len)
    }
}

/// Static layout of the register window for a function frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameLayout {
    hidden_count: RegisterIndex,
    parameter_count: RegisterIndex,
    local_count: RegisterIndex,
    temporary_count: RegisterIndex,
}

impl FrameLayout {
    /// Empty frame layout with no hidden, parameter, local, or temporary slots.
    pub const EMPTY: Self = Self {
        hidden_count: 0,
        parameter_count: 0,
        local_count: 0,
        temporary_count: 0,
    };

    /// Creates a checked frame layout.
    pub fn new(
        hidden_count: RegisterIndex,
        parameter_count: RegisterIndex,
        local_count: RegisterIndex,
        temporary_count: RegisterIndex,
    ) -> Result<Self, FrameLayoutError> {
        let total = u32::from(hidden_count)
            + u32::from(parameter_count)
            + u32::from(local_count)
            + u32::from(temporary_count);

        if total > u32::from(RegisterIndex::MAX) {
            return Err(FrameLayoutError::RegisterCountOverflow);
        }

        Ok(Self {
            hidden_count,
            parameter_count,
            local_count,
            temporary_count,
        })
    }

    /// Returns the number of hidden slots in the frame.
    #[must_use]
    pub const fn hidden_count(self) -> RegisterIndex {
        self.hidden_count
    }

    /// Returns the number of parameter slots in the frame.
    #[must_use]
    pub const fn parameter_count(self) -> RegisterIndex {
        self.parameter_count
    }

    /// Returns the number of local slots in the frame.
    #[must_use]
    pub const fn local_count(self) -> RegisterIndex {
        self.local_count
    }

    /// Returns the number of temporary slots in the frame.
    #[must_use]
    pub const fn temporary_count(self) -> RegisterIndex {
        self.temporary_count
    }

    /// Returns the total register count for the frame.
    #[must_use]
    pub const fn register_count(self) -> RegisterIndex {
        self.hidden_count
            .saturating_add(self.parameter_count)
            .saturating_add(self.local_count)
            .saturating_add(self.temporary_count)
    }

    /// Returns the index of the first user-visible register.
    #[must_use]
    pub const fn user_visible_start(self) -> RegisterIndex {
        self.hidden_count
    }

    /// Returns the number of user-visible registers in the frame.
    #[must_use]
    pub const fn user_visible_count(self) -> RegisterIndex {
        self.parameter_count
            .saturating_add(self.local_count)
            .saturating_add(self.temporary_count)
    }

    /// Returns the hidden-slot range.
    #[must_use]
    pub const fn hidden_range(self) -> RegisterRange {
        RegisterRange::new(0, self.hidden_count)
    }

    /// Returns the parameter-slot range.
    #[must_use]
    pub const fn parameter_range(self) -> RegisterRange {
        RegisterRange::new(self.hidden_count, self.parameter_count)
    }

    /// Returns the local-slot range.
    #[must_use]
    pub const fn local_range(self) -> RegisterRange {
        RegisterRange::new(self.parameter_range().end(), self.local_count)
    }

    /// Returns the temporary-slot range.
    #[must_use]
    pub const fn temporary_range(self) -> RegisterRange {
        RegisterRange::new(self.local_range().end(), self.temporary_count)
    }

    /// Returns `true` when the register index is user-visible to bytecode.
    #[must_use]
    pub const fn is_user_visible(self, register: RegisterIndex) -> bool {
        register >= self.user_visible_start() && register < self.register_count()
    }

    /// Resolves a bytecode-visible register index to an absolute frame index.
    #[must_use]
    pub const fn resolve_user_visible(self, register: RegisterIndex) -> Option<RegisterIndex> {
        if register < self.user_visible_count() {
            Some(self.user_visible_start().saturating_add(register))
        } else {
            None
        }
    }
}

impl Default for FrameLayout {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Per-activation flags shared by the interpreter and the future JIT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FrameFlags(u8);

impl FrameFlags {
    const CONSTRUCT: u8 = 1 << 0;
    const HAS_RECEIVER: u8 = 1 << 1;
    const MAY_SUSPEND: u8 = 1 << 2;

    /// Returns an empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns a flag set for the given frame properties.
    #[must_use]
    pub const fn new(is_construct: bool, has_receiver: bool, may_suspend: bool) -> Self {
        let mut bits = 0;

        if is_construct {
            bits |= Self::CONSTRUCT;
        }
        if has_receiver {
            bits |= Self::HAS_RECEIVER;
        }
        if may_suspend {
            bits |= Self::MAY_SUSPEND;
        }

        Self(bits)
    }

    /// Returns `true` if the frame is executing a construct call.
    #[must_use]
    pub const fn is_construct(self) -> bool {
        self.0 & Self::CONSTRUCT != 0
    }

    /// Returns `true` if the frame carries an explicit receiver.
    #[must_use]
    pub const fn has_receiver(self) -> bool {
        self.0 & Self::HAS_RECEIVER != 0
    }

    /// Returns `true` if the frame may suspend.
    #[must_use]
    pub const fn may_suspend(self) -> bool {
        self.0 & Self::MAY_SUSPEND != 0
    }
}

/// Dynamic metadata carried by a frame activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameMetadata {
    argument_count: RegisterIndex,
    flags: FrameFlags,
}

impl FrameMetadata {
    /// Creates activation metadata for a frame.
    #[must_use]
    pub const fn new(argument_count: RegisterIndex, flags: FrameFlags) -> Self {
        Self {
            argument_count,
            flags,
        }
    }

    /// Returns the actual argument count for the activation.
    #[must_use]
    pub const fn argument_count(self) -> RegisterIndex {
        self.argument_count
    }

    /// Returns the activation flags.
    #[must_use]
    pub const fn flags(self) -> FrameFlags {
        self.flags
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameFlags, FrameLayout, FrameLayoutError, FrameMetadata};

    #[test]
    fn frame_layout_computes_ranges() {
        let layout = FrameLayout::new(2, 3, 4, 5).expect("layout should be valid");

        assert_eq!(layout.hidden_range().start(), 0);
        assert_eq!(layout.hidden_range().end(), 2);
        assert_eq!(layout.parameter_range().start(), 2);
        assert_eq!(layout.parameter_range().end(), 5);
        assert_eq!(layout.local_range().start(), 5);
        assert_eq!(layout.local_range().end(), 9);
        assert_eq!(layout.temporary_range().start(), 9);
        assert_eq!(layout.temporary_range().end(), 14);
        assert_eq!(layout.register_count(), 14);
        assert!(!layout.is_user_visible(1));
        assert!(layout.is_user_visible(2));
        assert!(layout.is_user_visible(13));
    }

    #[test]
    fn frame_layout_rejects_overflow() {
        let result = FrameLayout::new(u16::MAX, 1, 0, 0);
        assert_eq!(result, Err(FrameLayoutError::RegisterCountOverflow));
    }

    #[test]
    fn frame_metadata_keeps_argument_count_and_flags() {
        let flags = FrameFlags::new(true, true, false);
        let metadata = FrameMetadata::new(3, flags);

        assert_eq!(metadata.argument_count(), 3);
        assert!(metadata.flags().is_construct());
        assert!(metadata.flags().has_receiver());
        assert!(!metadata.flags().may_suspend());
    }
}
