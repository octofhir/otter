//! The asm.js value-type lattice (asm.js §2.1, with the V8 errata).
//!
//! Every value type is a bitset that carries its own bit plus the bits of
//! all its supertypes, so `a.is_a(b)` — "`a` is a subtype of `b`" — is one
//! mask test. A heap view's element size and load/store types drive heap
//! access validation.
//!
//! # Contents
//! - [`AsmType`] — the lattice.
//! - [`HeapView`] — the typed-array views of the heap and their access types.
//! - [`Signature`] — the function type of an asm.js function or table.
//!
//! # Invariants
//! - The bit layout mirrors V8's `asm-types.h`: `FixNum ⊂ Signed ⊂ Int ⊂
//!   Intish`, `FixNum ⊂ Unsigned ⊂ Int`, `Float ⊂ FloatQ ⊂ Floatish`,
//!   `Double ⊂ DoubleQ`, `Signed, Double ⊂ Extern`.
//! - A [`Signature`] holds only the parameter types `Int`, `Double` and
//!   `Float`, and a return type of `Signed`, `Double`, `Float` or `Void`.
//!
//! # See also
//! - <http://asmjs.org/spec/latest/#value-types>
//! - `super::translate` — the validator that assigns these types.

use wasm_encoder::ValType;

/// One asm.js value type, as a subtype-closed bitset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct AsmType(u32);

const fn bit(n: u32) -> u32 {
    1 << n
}

const FLOATISH_DOUBLEQ: u32 = bit(2);
const FLOATQ_DOUBLEQ: u32 = bit(3);
const VOID: u32 = bit(4);
const EXTERN: u32 = bit(5);
const DOUBLEQ: u32 = bit(6) | FLOATISH_DOUBLEQ | FLOATQ_DOUBLEQ;
const DOUBLE: u32 = bit(7) | DOUBLEQ | EXTERN;
const INTISH: u32 = bit(8);
const INT: u32 = bit(9) | INTISH;
const SIGNED: u32 = bit(10) | INT | EXTERN;
const UNSIGNED: u32 = bit(11) | INT;
const FIXNUM: u32 = bit(12) | SIGNED | UNSIGNED;
const FLOATISH: u32 = bit(13) | FLOATISH_DOUBLEQ;
const FLOATQ: u32 = bit(14) | FLOATQ_DOUBLEQ | FLOATISH;
const FLOAT: u32 = bit(15) | FLOATQ;

impl AsmType {
    pub(super) const VOID: Self = Self(VOID);
    pub(super) const EXTERN: Self = Self(EXTERN);
    pub(super) const DOUBLEQ: Self = Self(DOUBLEQ);
    pub(super) const DOUBLE: Self = Self(DOUBLE);
    pub(super) const INTISH: Self = Self(INTISH);
    pub(super) const INT: Self = Self(INT);
    pub(super) const SIGNED: Self = Self(SIGNED);
    pub(super) const UNSIGNED: Self = Self(UNSIGNED);
    pub(super) const FIXNUM: Self = Self(FIXNUM);
    pub(super) const FLOATISH: Self = Self(FLOATISH);
    pub(super) const FLOATQ: Self = Self(FLOATQ);
    pub(super) const FLOAT: Self = Self(FLOAT);
    /// Store type of a `Float32Array` view: `floatish` or `double?`.
    pub(super) const FLOATISH_DOUBLEQ: Self = Self(FLOATISH_DOUBLEQ);
    /// Store type of a `Float64Array` view: `float?` or `double?`.
    pub(super) const FLOATQ_DOUBLEQ: Self = Self(FLOATQ_DOUBLEQ);

    /// `true` when `self` is a subtype of `that`.
    pub(super) const fn is_a(self, that: Self) -> bool {
        self.0 & that.0 == that.0
    }

    /// The wasm value type a value of this type travels as.
    pub(super) fn val_type(self) -> Option<ValType> {
        if self.is_a(Self::INTISH) {
            Some(ValType::I32)
        } else if self.is_a(Self::FLOATISH) {
            Some(ValType::F32)
        } else if self.is_a(Self::DOUBLEQ) {
            Some(ValType::F64)
        } else {
            None
        }
    }
}

/// The element kind of a heap view (`new stdlib.<View>(heap)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum HeapView {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
}

impl HeapView {
    /// The view whose stdlib constructor is named `name`.
    pub(super) fn from_constructor(name: &str) -> Option<Self> {
        Some(match name {
            "Int8Array" => Self::Int8,
            "Uint8Array" => Self::Uint8,
            "Int16Array" => Self::Int16,
            "Uint16Array" => Self::Uint16,
            "Int32Array" => Self::Int32,
            "Uint32Array" => Self::Uint32,
            "Float32Array" => Self::Float32,
            "Float64Array" => Self::Float64,
            _ => return None,
        })
    }

    /// Element size in bytes.
    pub(super) fn size(self) -> u32 {
        match self {
            Self::Int8 | Self::Uint8 => 1,
            Self::Int16 | Self::Uint16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }

    /// Type of a load from the view.
    pub(super) fn load_type(self) -> AsmType {
        match self {
            Self::Float32 => AsmType::FLOATQ,
            Self::Float64 => AsmType::DOUBLEQ,
            _ => AsmType::INTISH,
        }
    }

    /// Type a value stored to the view must have.
    pub(super) fn store_type(self) -> AsmType {
        match self {
            Self::Float32 => AsmType::FLOATISH_DOUBLEQ,
            Self::Float64 => AsmType::FLOATQ_DOUBLEQ,
            _ => AsmType::INTISH,
        }
    }
}

/// The function type of an asm.js function or function table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Signature {
    pub(super) params: Vec<AsmType>,
    pub(super) result: AsmType,
}

impl Signature {
    /// `true` when a call with argument types `args` expecting `result`
    /// may invoke a function of this type (V8 `CanBeInvokedWith`).
    pub(super) fn accepts(&self, result: AsmType, args: &[AsmType]) -> bool {
        self.result == result
            && self.params.len() == args.len()
            && args
                .iter()
                .zip(&self.params)
                .all(|(arg, param)| arg.is_a(*param))
    }

    /// The wasm parameter types.
    pub(super) fn wasm_params(&self) -> Vec<ValType> {
        self.params.iter().map(|ty| param_val_type(*ty)).collect()
    }

    /// The wasm result types: none for `void`.
    pub(super) fn wasm_results(&self) -> Vec<ValType> {
        if self.result == AsmType::VOID {
            Vec::new()
        } else {
            vec![param_val_type(self.result)]
        }
    }
}

/// The wasm type of a parameter or result of type `Int`/`Signed`, `Float` or
/// `Double`.
fn param_val_type(ty: AsmType) -> ValType {
    if ty.is_a(AsmType::DOUBLE) {
        ValType::F64
    } else if ty.is_a(AsmType::FLOAT) {
        ValType::F32
    } else {
        ValType::I32
    }
}
