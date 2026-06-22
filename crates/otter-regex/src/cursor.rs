//! Input traversal over UTF-16 code units, code-point-aware in Unicode mode.
//!
//! Otter subjects are UTF-16, so UTF-16 is the **native** input representation.
//! In non-Unicode mode the engine matches per code unit; in Unicode (`u`/`v`)
//! mode it decodes surrogate pairs and matches per code point — but **always
//! reports code-unit offsets** so the host can slice its `&[u16]` directly.
//!
//! # Contents
//! - [`Input`] — a borrowed subject plus its mode (code-unit vs code-point).
//!
//! # Invariants
//! - Positions are UTF-16 code-unit indices in `0..=len`.
//! - In code-point mode, advancing over a well-formed surrogate pair moves the
//!   position by 2 code units and yields the combined code point; lone
//!   surrogates yield their own value (per §22.2.2 Unicode handling).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (Unicode handling notes)

/// A borrowed UTF-16 subject and the traversal mode for one execution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input<'t> {
    units: &'t [u16],
    /// When `true`, surrogate pairs decode to a single code point.
    code_point_mode: bool,
}

impl<'t> Input<'t> {
    /// Wrap a UTF-16 subject. `code_point_mode` is set when the regex has the
    /// `u` or `v` flag.
    #[must_use]
    pub(crate) fn new(units: &'t [u16], code_point_mode: bool) -> Self {
        Self {
            units,
            code_point_mode,
        }
    }

    /// Whether traversal decodes surrogate pairs to code points.
    #[must_use]
    pub(crate) fn is_code_point_mode(&self) -> bool {
        self.code_point_mode
    }

    /// The backing code units.
    #[must_use]
    pub(crate) fn units(&self) -> &'t [u16] {
        self.units
    }
}
