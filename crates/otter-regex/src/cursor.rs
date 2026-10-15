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
    /// `true` for the two-byte UTF-16 width.
    const WIDE: bool;

    /// The unit as a UTF-16 code unit.
    fn widen(self) -> u16;

    /// `unit` at this width, or `None` when it does not fit — a unit no
    /// one-byte subject can contain.
    fn narrow(unit: u16) -> Option<Self>;

    /// Whether `window` holds exactly the UTF-16 units of `seq`, which has the
    /// same length.
    fn eq_units(window: &[Self], seq: &[u16]) -> bool;

    /// Offset of the first `needle` in `haystack`, scanned several units at a
    /// time.
    fn position(haystack: &[Self], needle: Self) -> Option<usize>;

    /// The units as bytes, for a one-byte subject.
    fn as_latin1(units: &[Self]) -> Option<&[u8]>;

    /// The units' memory as bytes, little-endian per two-byte unit on the
    /// targets the engine runs on.
    fn as_bytes(units: &[Self]) -> &[u8];
}

impl Unit for u8 {
    const WIDE: bool = false;

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

    #[inline]
    fn position(haystack: &[Self], needle: Self) -> Option<usize> {
        memchr::memchr(needle, haystack)
    }

    #[inline]
    fn as_latin1(units: &[Self]) -> Option<&[u8]> {
        Some(units)
    }

    #[inline]
    fn as_bytes(units: &[Self]) -> &[u8] {
        units
    }
}

impl Unit for u16 {
    const WIDE: bool = true;

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

    /// Four units per 64-bit word: a lane equal to `needle` is a zero lane of
    /// `word ^ pattern`, and the lowest set high bit of the borrow trick
    /// marks the first one exactly.
    #[inline]
    fn position(haystack: &[Self], needle: Self) -> Option<usize> {
        const LOW: u64 = 0x0001_0001_0001_0001;
        const HIGH: u64 = 0x8000_8000_8000_8000;
        let pattern = LOW * u64::from(needle);
        let (chunks, remainder) = haystack.as_chunks::<4>();
        let mut base = 0;
        for chunk in chunks {
            let word = u64::from(chunk[0])
                | (u64::from(chunk[1]) << 16)
                | (u64::from(chunk[2]) << 32)
                | (u64::from(chunk[3]) << 48);
            let lanes = word ^ pattern;
            let zero = lanes.wrapping_sub(LOW) & !lanes & HIGH;
            if zero != 0 {
                return Some(base + (zero.trailing_zeros() / 16) as usize);
            }
            base += 4;
        }
        remainder
            .iter()
            .position(|&unit| unit == needle)
            .map(|offset| base + offset)
    }

    #[inline]
    fn as_latin1(_units: &[Self]) -> Option<&[u8]> {
        None
    }

    #[inline]
    fn as_bytes(units: &[Self]) -> &[u8] {
        // SAFETY: every byte of a `u16` slice is initialized, `u8` has no
        // alignment requirement, and the byte view covers exactly the
        // slice's memory for the slice's lifetime.
        unsafe { core::slice::from_raw_parts(units.as_ptr().cast::<u8>(), units.len() * 2) }
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

#[cfg(test)]
mod tests {
    use super::Unit;

    fn naive<U: Unit>(haystack: &[U], needle: U) -> Option<usize> {
        haystack.iter().position(|&unit| unit == needle)
    }

    #[test]
    fn word_scans_find_the_first_unit_at_every_lane_and_remainder() {
        for len in 0..23 {
            for at in 0..=len {
                let mut wide: Vec<u16> = (0..len).map(|i| 0x4100 + i as u16).collect();
                let mut narrow: Vec<u8> = (0..len).map(|i| b'a' + i as u8).collect();
                if at < len {
                    wide[at] = 0x00ff;
                    narrow[at] = b'#';
                    // A later duplicate never hides the first.
                    if at + 2 < len {
                        wide[at + 2] = 0x00ff;
                        narrow[at + 2] = b'#';
                    }
                }
                assert_eq!(u16::position(&wide, 0x00ff), naive(&wide, 0x00ff));
                assert_eq!(u8::position(&narrow, b'#'), naive(&narrow, b'#'));
            }
        }
        // Lanes that differ from the needle only by a borrow are not matches.
        let tricky = [0x0001, 0x0000, 0x8000, 0x7fff, 0x0100, 0x0001];
        for needle in [0x0000, 0x0001, 0x8000, 0x7fff, 0x0100, 0xffff] {
            assert_eq!(u16::position(&tricky, needle), naive(&tricky, needle));
        }
    }
}
