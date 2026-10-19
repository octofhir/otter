//! Lossless transport of a WTF-16 source string through a UTF-8 parser.
//!
//! `eval`, indirect `eval`, and the `Function` constructor take their source
//! from a JS string, which is WTF-16 and may hold an unpaired surrogate. The
//! parser takes `&str`, which cannot: converting lossily turns every unpaired
//! surrogate into U+FFFD, so `eval("/" + "\\" + String.fromCharCode(0xD800) +
//! "/").source` came back with the wrong code unit — and so did every string
//! and template literal spelled with a raw surrogate.
//!
//! The escape below carries those code units through the parser and restores
//! them in the compiled module. Literal text is the only place an unpaired
//! surrogate can legally appear (it is not an identifier character), and every
//! literal reaches the runtime as a WTF-16 constant, so the two ends of the
//! escape are [`encode`] over the source and [`decode_module`] over the
//! constants.
//!
//! # Contents
//! - [`encode`] — WTF-16 source to parser-ready UTF-8.
//! - [`decode_module`] — restore the escaped code units in a compiled module.
//!
//! # Invariants
//! - The escape is [`crate::wtf16_escape`]'s: self-delimiting, so every
//!   marker in the encoded text introduces exactly one escape.
//! - Encoding is unconditional, so sources assembled from several encoded
//!   pieces (the `Function` constructor's parameters and body) decode as one.
//! - A well-formed surrogate pair is left alone; only unpaired halves escape.
//!
//! # See also
//! - `eval_ops` — the three call sites that own both ends of the escape.

use crate::wtf16_escape::{self, decode_units};
use otter_bytecode::{BytecodeModule, Constant};

/// Encode a WTF-16 source string as UTF-8 the parser can accept.
#[must_use]
pub(crate) fn encode(units: &[u16]) -> String {
    wtf16_escape::encode(units)
}

/// Restore escaped code units in every literal a compiled module carries.
pub(crate) fn decode_module(module: &mut BytecodeModule) {
    for constant in &mut module.constants {
        match constant {
            Constant::String { utf16 } => decode_units(utf16),
            Constant::RegExp { pattern_utf16, .. } => decode_units(pattern_utf16),
            Constant::ArrayBoilerplate { elements } => {
                for element in elements {
                    if let otter_bytecode::BoilerplateElement::String { utf16 } = element {
                        decode_units(utf16);
                    }
                }
            }
            Constant::Number { .. } | Constant::FunctionId { .. } | Constant::BigInt { .. } => {}
        }
    }
    if let Some(function_source) = module.function_source.as_mut() {
        decode_source_text(function_source);
    }
}

/// Undo the escape in [[SourceText]], which is a `String` and so cannot hold
/// the restored surrogate. An escaped half becomes U+FFFD, which is what
/// `Function.prototype.toString` reported before the escape existed; a doubled
/// marker still collapses to the single character the author wrote.
fn decode_source_text(text: &mut String) {
    if !text.contains('\u{E000}') {
        return;
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    decode_units(&mut units);
    *text = String::from_utf16_lossy(&units);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(units: &[u16]) -> Vec<u16> {
        let encoded = encode(units);
        let mut decoded: Vec<u16> = encoded.encode_utf16().collect();
        decode_units(&mut decoded);
        decoded
    }

    #[test]
    fn unpaired_surrogates_survive_the_parser_encoding() {
        for unit in [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF] {
            let source = ['/' as u16, '\\' as u16, unit, '/' as u16];
            assert!(!encode(&source).contains('\u{FFFD}'));
            assert_eq!(round_trip(&source), source);
        }
    }

    #[test]
    fn marker_and_pairs_are_left_as_the_author_wrote_them() {
        // A literal marker in a source that also needs escaping.
        let source: Vec<u16> = "a\u{E000}b".encode_utf16().chain([0xD800]).collect();
        assert_eq!(round_trip(&source), source);
        // A well-formed pair is not an escape, and a source without either is
        // handed to the parser untouched.
        let pair: Vec<u16> = "x\u{1F600}y".encode_utf16().collect();
        assert_eq!(encode(&pair), "x\u{1F600}y");
        assert_eq!(round_trip(&pair), pair);
    }
}
