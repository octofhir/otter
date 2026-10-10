//! Input traversal over a subject's code units, code-point-aware in Unicode
//! mode.
//!
//! A JavaScript string is a sequence of UTF-16 code units, but most strings
//! only hold units below `0x100` and are stored one byte per unit. The matcher
//! runs over either width directly — [`Unit`] is the one abstraction — so a
//! one-byte subject is never widened to search it. In non-Unicode mode the
//! engine matches per code unit; in Unicode (`u`/`v`) mode it decodes surrogate
//! pairs and matches per code point — but **always reports code-unit offsets**
//! so the host can slice the subject directly.
//!
//! # Contents
//! - [`Unit`] — one code unit of a subject: a Latin-1 byte or a UTF-16 unit.
//! - [`Input`] — a borrowed subject plus its mode (code-unit vs code-point).
//!
//! # Invariants
//! - Positions are code-unit indices in `0..=len`, identical for both widths:
//!   a Latin-1 byte is exactly one UTF-16 code unit.
//! - In code-point mode, advancing over a well-formed surrogate pair moves the
//!   position by 2 code units and yields the combined code point; lone
//!   surrogates yield their own value (per §22.2.2 Unicode handling). A
//!   one-byte subject holds no surrogates, so it decodes unit by unit.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (Unicode handling notes)

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
}

/// One code unit of a subject: a Latin-1 byte (`u8`) or a UTF-16 unit (`u16`).
///
/// Sealed: the matcher is monomorphized for exactly these two widths.
pub trait Unit: sealed::Sealed + Copy + Eq + core::fmt::Debug + Into<u32> + 'static {
    /// The unit as a UTF-16 code unit.
    fn widen(self) -> u16;

    /// `unit` at this width, or `None` when it does not fit — a unit no
    /// one-byte subject can contain.
    fn narrow(unit: u16) -> Option<Self>;

    /// Whether `window` holds exactly the UTF-16 units of `seq`, which has the
    /// same length.
    fn eq_units(window: &[Self], seq: &[u16]) -> bool;
}

impl Unit for u8 {
    #[inline]
    fn widen(self) -> u16 {
        u16::from(self)
    }

    #[inline]
    fn narrow(unit: u16) -> Option<Self> {
        u8::try_from(unit).ok()
    }

    #[inline]
    fn eq_units(window: &[Self], seq: &[u16]) -> bool {
        debug_assert_eq!(window.len(), seq.len());
        window.iter().zip(seq).all(|(&a, &b)| u16::from(a) == b)
    }
}

impl Unit for u16 {
    #[inline]
    fn widen(self) -> u16 {
        self
    }

    #[inline]
    fn narrow(unit: u16) -> Option<Self> {
        Some(unit)
    }

    #[inline]
    fn eq_units(window: &[Self], seq: &[u16]) -> bool {
        debug_assert_eq!(window.len(), seq.len());
        // Short runs — nearly all of them — compare unit by unit so the check
        // stays inline instead of paying a call into the slice comparison;
        // longer runs are worth that call.
        match seq.len() {
            2 => window[0] == seq[0] && window[1] == seq[1],
            3 => window[0] == seq[0] && window[1] == seq[1] && window[2] == seq[2],
            4 => {
                window[0] == seq[0]
                    && window[1] == seq[1]
                    && window[2] == seq[2]
                    && window[3] == seq[3]
            }
            _ => window == seq,
        }
    }
}

/// A borrowed subject and the traversal mode for one execution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input<'t, U: Unit> {
    units: &'t [U],
    /// When `true`, surrogate pairs decode to a single code point.
    code_point_mode: bool,
}

impl<'t, U: Unit> Input<'t, U> {
    /// Wrap a subject. `code_point_mode` is set when the regex has the `u` or
    /// `v` flag.
    #[must_use]
    pub(crate) fn new(units: &'t [U], code_point_mode: bool) -> Self {
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
    pub(crate) fn units(&self) -> &'t [U] {
        self.units
    }
}
