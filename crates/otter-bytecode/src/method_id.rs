//! Typed method-id enums shared by the compiler and the VM.
//!
//! Variadic `Op::*Call` opcodes encode the dispatched method name
//! as `Operand::ConstIndex(u32)` where the `u32` is the
//! discriminant of the corresponding enum below. The compiler
//! uses [`from_str`](Self::from_str) at lowering time; the
//! interpreter casts back via [`from_u32`](Self::from_u32).
//!
//! Replacing the previous string-pool indirection eliminates the
//! per-call `match name { "x" => … }` chain in the hot dispatch
//! loop and removes an O(N) `lookup_string_constant` step.
//!
//! # Invariants
//! - Enum discriminants are written explicitly; never reorder
//!   variants without also bumping any persistent bytecode
//!   serialisation format.
//! - Every enum exposes
//!   [`from_str`](`JsonMethod::from_str`) (compile-time mapping)
//!   and [`from_u32`](`JsonMethod::from_u32`) (runtime decode).

macro_rules! method_id_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $variant:ident = $disc:expr => $str:expr, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(missing_docs)]
        #[repr(u32)]
        pub enum $name {
            $( $variant = $disc, )+
        }

        #[allow(clippy::should_implement_trait)]
        impl $name {
            /// Map a JS-visible method name to its typed id at
            /// compile time. Returns `None` for unknown names so
            /// the compiler can either reject or fall through.
            #[must_use]
            pub fn from_str(name: &str) -> Option<Self> {
                match name {
                    $( $str => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// Decode a discriminant produced by [`as_u32`](Self::as_u32).
            #[must_use]
            pub fn from_u32(value: u32) -> Option<Self> {
                match value {
                    $( $disc => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// Encode as the `u32` carried by `Operand::ConstIndex`.
            #[must_use]
            #[inline]
            pub fn as_u32(self) -> u32 {
                self as u32
            }

            /// JS-visible method name (for diagnostics).
            #[must_use]
            pub fn name(self) -> &'static str {
                match self {
                    $( Self::$variant => $str, )+
                }
            }
        }
    };
}

method_id_enum! {
    /// Methods reached through [`Op::JsonCall`](crate::Op::JsonCall).
    JsonMethod {
        Parse = 0 => "parse",
        Stringify = 1 => "stringify",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::MathCall`](crate::Op::MathCall).
    /// Pure functions only — namespace constants (`PI`, `E`, …)
    /// load through [`Op::MathLoad`](crate::Op::MathLoad).
    MathMethod {
        Abs = 0 => "abs",
        Acos = 1 => "acos",
        Acosh = 2 => "acosh",
        Asin = 3 => "asin",
        Asinh = 4 => "asinh",
        Atan = 5 => "atan",
        Atan2 = 6 => "atan2",
        Atanh = 7 => "atanh",
        Cbrt = 8 => "cbrt",
        Ceil = 9 => "ceil",
        Clz32 = 10 => "clz32",
        Cos = 11 => "cos",
        Cosh = 12 => "cosh",
        Exp = 13 => "exp",
        Expm1 = 14 => "expm1",
        Floor = 15 => "floor",
        Fround = 16 => "fround",
        Hypot = 17 => "hypot",
        Imul = 18 => "imul",
        Log = 19 => "log",
        Log10 = 20 => "log10",
        Log1p = 21 => "log1p",
        Log2 = 22 => "log2",
        Max = 23 => "max",
        Min = 24 => "min",
        Pow = 25 => "pow",
        Random = 26 => "random",
        Round = 27 => "round",
        Sign = 28 => "sign",
        Sin = 29 => "sin",
        Sinh = 30 => "sinh",
        Sqrt = 31 => "sqrt",
        Tan = 32 => "tan",
        Tanh = 33 => "tanh",
        Trunc = 34 => "trunc",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::ObjectCall`](crate::Op::ObjectCall).
    ObjectMethod {
        Assign = 0 => "assign",
        Create = 1 => "create",
        DefineProperty = 2 => "defineProperty",
        DefineProperties = 3 => "defineProperties",
        Entries = 4 => "entries",
        Freeze = 5 => "freeze",
        FromEntries = 6 => "fromEntries",
        GetOwnPropertyDescriptor = 7 => "getOwnPropertyDescriptor",
        GetOwnPropertyDescriptors = 8 => "getOwnPropertyDescriptors",
        GetOwnPropertyNames = 9 => "getOwnPropertyNames",
        GetOwnPropertySymbols = 10 => "getOwnPropertySymbols",
        HasOwn = 11 => "hasOwn",
        IsExtensible = 12 => "isExtensible",
        IsFrozen = 13 => "isFrozen",
        IsSealed = 14 => "isSealed",
        Keys = 15 => "keys",
        PreventExtensions = 16 => "preventExtensions",
        Seal = 17 => "seal",
        Values = 18 => "values",
        GroupBy = 19 => "groupBy",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::GlobalCall`](crate::Op::GlobalCall).
    /// Global host functions plus the `Number.<predicate>` static
    /// surface that aliases / strictifies them.
    GlobalMethod {
        ParseInt = 0 => "parseInt",
        ParseFloat = 1 => "parseFloat",
        IsFinite = 2 => "isFinite",
        IsNaN = 3 => "isNaN",
        EncodeURI = 4 => "encodeURI",
        DecodeURI = 5 => "decodeURI",
        EncodeURIComponent = 6 => "encodeURIComponent",
        DecodeURIComponent = 7 => "decodeURIComponent",
        NumberIsNaN = 8 => "Number.isNaN",
        NumberIsFinite = 9 => "Number.isFinite",
        NumberIsInteger = 10 => "Number.isInteger",
        NumberIsSafeInteger = 11 => "Number.isSafeInteger",
        Escape = 12 => "escape",
        Unescape = 13 => "unescape",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::SymbolCall`](crate::Op::SymbolCall).
    /// `Construct` is the bare `Symbol(desc)` call shape.
    SymbolMethod {
        Construct = 0 => "",
        For = 1 => "for",
        KeyFor = 2 => "keyFor",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::PromiseCall`](crate::Op::PromiseCall).
    PromiseMethod {
        Resolve = 0 => "resolve",
        Reject = 1 => "reject",
        All = 2 => "all",
        AllSettled = 3 => "allSettled",
        Any = 4 => "any",
        Race = 5 => "race",
        WithResolvers = 6 => "withResolvers",
        Try = 7 => "try",
        AllKeyed = 8 => "allKeyed",
        AllSettledKeyed = 9 => "allSettledKeyed",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::BigIntCall`](crate::Op::BigIntCall).
    /// Includes the constructor (empty name in the previous string
    /// scheme — encoded as `Construct` here).
    BigIntMethod {
        Construct = 0 => "",
        AsIntN = 1 => "asIntN",
        AsUintN = 2 => "asUintN",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::DateCall`](crate::Op::DateCall).
    /// Includes the constructor (`Construct`) plus the static
    /// surface.
    DateMethod {
        Construct = 0 => "",
        Now = 1 => "now",
        Parse = 2 => "parse",
        UTC = 3 => "UTC",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::StringCall`](crate::Op::StringCall).
    /// Includes the constructor (empty name) plus the static surface.
    StringMethod {
        Construct = 0 => "",
        FromCharCode = 1 => "fromCharCode",
        FromCodePoint = 2 => "fromCodePoint",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::ArrayBufferCall`](crate::Op::ArrayBufferCall).
    /// Includes the constructor (empty name) and `isView`.
    ArrayBufferMethod {
        Construct = 0 => "",
        IsView = 1 => "isView",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::DataViewCall`](crate::Op::DataViewCall).
    /// Constructor only.
    DataViewMethod {
        Construct = 0 => "",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::SharedArrayBufferCall`](crate::Op::SharedArrayBufferCall).
    /// Constructor only.
    SharedArrayBufferMethod {
        Construct = 0 => "",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::AtomicsCall`](crate::Op::AtomicsCall).
    AtomicsMethod {
        Add = 0 => "add",
        And = 1 => "and",
        CompareExchange = 2 => "compareExchange",
        Exchange = 3 => "exchange",
        IsLockFree = 4 => "isLockFree",
        Load = 5 => "load",
        Notify = 6 => "notify",
        Or = 7 => "or",
        Store = 8 => "store",
        Sub = 9 => "sub",
        Wait = 10 => "wait",
        WaitAsync = 11 => "waitAsync",
        Xor = 12 => "xor",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::ProxyCall`](crate::Op::ProxyCall).
    ProxyMethod {
        Construct = 0 => "",
        Revocable = 1 => "revocable",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::ReflectCall`](crate::Op::ReflectCall).
    ReflectMethod {
        Apply = 0 => "apply",
        Construct = 1 => "construct",
        DefineProperty = 2 => "defineProperty",
        DeleteProperty = 3 => "deleteProperty",
        Get = 4 => "get",
        GetOwnPropertyDescriptor = 5 => "getOwnPropertyDescriptor",
        GetPrototypeOf = 6 => "getPrototypeOf",
        Has = 7 => "has",
        IsExtensible = 8 => "isExtensible",
        OwnKeys = 9 => "ownKeys",
        PreventExtensions = 10 => "preventExtensions",
        Set = 11 => "set",
        SetPrototypeOf = 12 => "setPrototypeOf",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::IteratorCall`](crate::Op::IteratorCall).
    IteratorMethod {
        Construct = 0 => "",
        From = 1 => "from",
    }
}

method_id_enum! {
    /// Methods reached through [`Op::TypedArrayCall`](crate::Op::TypedArrayCall).
    /// The leading operand carries the kind discriminant; this enum
    /// covers the trailing static surface (`Construct` / `from` /
    /// `of`).
    TypedArrayMethod {
        Construct = 0 => "",
        From = 1 => "from",
        Of = 2 => "of",
    }
}

method_id_enum! {
    /// Identifiers of the eleven concrete TypedArray kinds. Mirrors
    /// [`otter_vm::binary::TypedArrayKind`] so the compiler can emit
    /// the discriminant directly without depending on `otter-vm`. The
    /// discriminants must stay in sync with the runtime enum.
    TypedArrayKindId {
        Int8 = 0 => "Int8Array",
        Uint8 = 1 => "Uint8Array",
        Uint8Clamped = 2 => "Uint8ClampedArray",
        Int16 = 3 => "Int16Array",
        Uint16 = 4 => "Uint16Array",
        Int32 = 5 => "Int32Array",
        Uint32 = 6 => "Uint32Array",
        Float32 = 7 => "Float32Array",
        Float64 = 8 => "Float64Array",
        BigInt64 = 9 => "BigInt64Array",
        BigUint64 = 10 => "BigUint64Array",
    }
}

method_id_enum! {
    /// Class identifiers carried as the leading operand of
    /// [`Op::TemporalCall`](crate::Op::TemporalCall).
    TemporalClassId {
        Instant = 0 => "Instant",
        Duration = 1 => "Duration",
        PlainDate = 2 => "PlainDate",
        PlainTime = 3 => "PlainTime",
        PlainDateTime = 4 => "PlainDateTime",
        Now = 5 => "Now",
    }
}

method_id_enum! {
    /// Static methods reached through [`Op::TemporalCall`](crate::Op::TemporalCall).
    /// Union of every per-class static surface; the runtime gates
    /// each variant against the leading [`TemporalClassId`] operand.
    TemporalMethod {
        From = 0 => "from",
        Compare = 1 => "compare",
        FromEpochMilliseconds = 2 => "fromEpochMilliseconds",
        NowInstant = 3 => "instant",
        NowPlainDateTimeISO = 4 => "plainDateTimeISO",
        NowPlainDateISO = 5 => "plainDateISO",
        NowPlainTimeISO = 6 => "plainTimeISO",
    }
}
