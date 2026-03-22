//! MIR type system.
//!
//! MIR values are explicitly typed. Guards narrow `Boxed` values into
//! unboxed representations. The type system is intentionally small —
//! it tracks what the JIT can meaningfully specialize on.

/// Type of a MIR value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MirType {
    /// NaN-boxed JS value (u64). Default for unspecialized code.
    Boxed,
    /// Unboxed i32 (proven by `GuardInt32` or constant).
    Int32,
    /// Unboxed f64 (proven by `GuardFloat64` or constant).
    Float64,
    /// Unboxed boolean (from comparison results or `GuardBool`).
    Bool,
    /// Raw pointer to JsObject (proven by `GuardObject`).
    ObjectRef,
    /// Raw pointer to JsString (proven by `GuardString`).
    StringRef,
    /// Raw pointer to Closure (proven by `GuardFunction`).
    FunctionRef,
    /// ObjectRef known to be a dense array (proven by `GuardArrayDense`).
    ArrayRef,
    /// No value. Used for stores, safepoints, void operations.
    Void,
}

impl MirType {
    /// Whether this type represents a GC-managed heap pointer.
    pub fn is_gc_ref(self) -> bool {
        matches!(
            self,
            MirType::ObjectRef
                | MirType::StringRef
                | MirType::FunctionRef
                | MirType::ArrayRef
        )
    }

    /// Whether this type may contain a GC-managed pointer (Boxed can).
    pub fn may_contain_gc_ref(self) -> bool {
        self.is_gc_ref() || self == MirType::Boxed
    }

    /// Whether this is an unboxed numeric type.
    pub fn is_numeric(self) -> bool {
        matches!(self, MirType::Int32 | MirType::Float64)
    }

    /// Short display name for diagnostics.
    pub fn short_name(self) -> &'static str {
        match self {
            MirType::Boxed => "box",
            MirType::Int32 => "i32",
            MirType::Float64 => "f64",
            MirType::Bool => "bool",
            MirType::ObjectRef => "obj",
            MirType::StringRef => "str",
            MirType::FunctionRef => "func",
            MirType::ArrayRef => "arr",
            MirType::Void => "void",
        }
    }
}

impl std::fmt::Display for MirType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short_name())
    }
}

/// Comparison operator for typed comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl std::fmt::Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmpOp::Eq => write!(f, "=="),
            CmpOp::Ne => write!(f, "!="),
            CmpOp::Lt => write!(f, "<"),
            CmpOp::Le => write!(f, "<="),
            CmpOp::Gt => write!(f, ">"),
            CmpOp::Ge => write!(f, ">="),
        }
    }
}
