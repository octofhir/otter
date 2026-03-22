//! Runtime feedback side-table placeholders.

/// Marker for the feedback table associated with a function in the new VM.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackLayout;
