//! Executable module container for the new VM.

use crate::frame::FrameLayout;

/// Minimal executable module placeholder for the new VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    frame_layout: FrameLayout,
}

impl Module {
    /// Creates a new module with the provided frame layout.
    #[must_use]
    pub const fn new(frame_layout: FrameLayout) -> Self {
        Self { frame_layout }
    }

    /// Returns the frame layout associated with the module entry point.
    #[must_use]
    pub const fn frame_layout(self) -> FrameLayout {
        self.frame_layout
    }
}
