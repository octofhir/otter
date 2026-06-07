//! Literal expression lowering.
//!
//! # Contents
//! - [`compile_string_literal`] — lowers string literals.
//! - [`compile_bigint_literal`] — lowers bigint literals.
//! - [`compile_regexp_literal`] — lowers regular expression literals.
//! - [`compile_numeric_literal`] — lowers numeric literals.
//! - [`compile_boolean_literal`] — lowers boolean literals.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::{BigIntLiteral, BooleanLiteral, NumericLiteral, RegExpLiteral, StringLiteral};

pub(crate) fn compile_string_literal(
    cx: &mut Compiler,
    lit: &StringLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let dst = cx.alloc_scratch();
    let const_idx = if lit.lone_surrogates {
        let utf16 = decode_lone_surrogate_string(&lit.value);
        cx.intern_utf16_string_constant(utf16)
    } else {
        cx.intern_string_constant(&lit.value)
    };
    cx.emit(
        Op::LoadString,
        [Operand::Register(dst), Operand::ConstIndex(const_idx)],
        (lit.span.start, lit.span.end),
    );
    Ok(dst)
}

/// §13.2.5.5 — a `BigInt` literal used as a property key becomes the
/// string `ToString(BigInt)` (always decimal, base-independent). oxc
/// normalizes `lit.value` to the digit text; reformatting through
/// `num_bigint` collapses any radix prefix to canonical decimal.
pub(crate) fn bigint_literal_property_name(lit: &BigIntLiteral<'_>) -> Option<String> {
    lit.value
        .as_str()
        .parse::<num_bigint::BigInt>()
        .ok()
        .map(|b| b.to_string())
}

pub(crate) fn compile_bigint_literal(
    cx: &mut Compiler,
    lit: &BigIntLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (lit.span.start, lit.span.end);
    let dst = cx.alloc_scratch();
    let decimal = lit.value.as_str().to_string();
    // Compile-time syntactic validation so the runtime
    // parse path can stay strict (treats failure as
    // `InvalidOperand` rather than a surfaced parse error).
    if decimal.parse::<num_bigint::BigInt>().is_err() {
        return Err(CompileError::Unsupported {
            node: format!("BigIntLiteral with non-decimal payload `{decimal}`"),
            span,
        });
    }
    let const_idx = cx.intern_bigint_constant(&decimal);
    cx.emit(
        Op::LoadBigInt,
        [Operand::Register(dst), Operand::ConstIndex(const_idx)],
        span,
    );
    Ok(dst)
}

pub(crate) fn compile_regexp_literal(
    cx: &mut Compiler,
    lit: &RegExpLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let span = (lit.span.start, lit.span.end);
    let pattern_text = lit.regex.pattern.text.as_str();
    let flags_str = lit.regex.flags.to_string();
    // Compile-time validation: feed the pattern + flags to
    // `regress` so we surface a clean `Unsupported` for the
    // few patterns the engine rejects (e.g. unterminated
    // groups). Mirrors the BigIntLiteral approach. The `g`,
    // `y`, and `d` flags live above the matcher per JS spec
    // (§22.2.6.4 [`get RegExp.prototype.flags`](https://tc39.es/ecma262/#sec-get-regexp.prototype.flags)),
    // so we strip them before asking `regress` to compile.
    let mut engine_flags = regress::Flags::default();
    let mut saw_u = false;
    let mut saw_v = false;
    for c in flags_str.chars() {
        match c {
            'd' | 'g' | 'y' => {}
            'i' => engine_flags.icase = true,
            'm' => engine_flags.multiline = true,
            's' => engine_flags.dot_all = true,
            'u' => {
                engine_flags.unicode = true;
                saw_u = true;
            }
            'v' => {
                engine_flags.unicode_sets = true;
                saw_v = true;
            }
            other => {
                return Err(CompileError::Unsupported {
                    node: format!(
                        "RegExpLiteral `/{pattern_text}/{flags_str}` has unsupported flag `{other}`"
                    ),
                    span,
                });
            }
        }
    }
    if saw_u && saw_v {
        return Err(CompileError::Unsupported {
            node: format!(
                "RegExpLiteral `/{pattern_text}/{flags_str}` rejected: flags `u` and `v` are mutually exclusive (§22.2.4)"
            ),
            span,
        });
    }
    if let Err(e) = regress::Regex::with_flags(pattern_text, engine_flags) {
        return Err(CompileError::Unsupported {
            node: format!("RegExpLiteral `/{pattern_text}/{flags_str}` rejected: {e}"),
            span,
        });
    }
    let pattern_utf16: Vec<u16> = pattern_text.encode_utf16().collect();
    let dst = cx.alloc_scratch();
    let const_idx = cx.intern_regexp_constant(&pattern_utf16, &flags_str);
    cx.emit(
        Op::LoadRegExp,
        [Operand::Register(dst), Operand::ConstIndex(const_idx)],
        span,
    );
    Ok(dst)
}

pub(crate) fn compile_numeric_literal(
    cx: &mut Compiler,
    lit: &NumericLiteral<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let dst = cx.alloc_scratch();
    let span = (lit.span.start, lit.span.end);
    // Smi fast path: integer-valued literal in i32 range.
    if lit.value.fract() == 0.0
        && lit.value.is_finite()
        && (i32::MIN as f64..=i32::MAX as f64).contains(&lit.value)
        && !(lit.value == 0.0 && lit.value.is_sign_negative())
    {
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(dst), Operand::Imm32(lit.value as i32)],
            span,
        );
    } else {
        let const_idx = cx.intern_number_constant(lit.value);
        cx.emit(
            Op::LoadNumber,
            [Operand::Register(dst), Operand::ConstIndex(const_idx)],
            span,
        );
    }
    Ok(dst)
}

pub(crate) fn compile_boolean_literal(
    cx: &mut Compiler,
    lit: &BooleanLiteral,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let dst = cx.alloc_scratch();
    let span = (lit.span.start, lit.span.end);
    cx.emit(
        if lit.value {
            Op::LoadTrue
        } else {
            Op::LoadFalse
        },
        [Operand::Register(dst)],
        span,
    );
    Ok(dst)
}
