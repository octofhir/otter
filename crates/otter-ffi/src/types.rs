//! FFI type system — C type descriptors and function signatures.

/// C type descriptors for FFI argument and return types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FFIType {
    Char = 0,
    I8 = 1,
    U8 = 2,
    I16 = 3,
    U16 = 4,
    I32 = 5,
    U32 = 6,
    I64 = 7,
    U64 = 8,
    F64 = 9,
    F32 = 10,
    Bool = 11,
    Ptr = 12,
    Void = 13,
    CString = 14,
    I64Fast = 15,
    U64Fast = 16,
    Function = 17,
}

impl FFIType {
    /// Parse a type name string into an FFIType.
    ///
    /// Accepts shorthand names ("i32", "cstring") and C-style names ("int32_t", "int").
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "char" => Some(Self::Char),
            "i8" | "int8_t" => Some(Self::I8),
            "u8" | "uint8_t" => Some(Self::U8),
            "i16" | "int16_t" => Some(Self::I16),
            "u16" | "uint16_t" => Some(Self::U16),
            "i32" | "int32_t" | "int" => Some(Self::I32),
            "u32" | "uint32_t" => Some(Self::U32),
            "i64" | "int64_t" | "i64_fast" => Some(Self::I64),
            "u64" | "uint64_t" | "usize" | "u64_fast" => Some(Self::U64),
            "f64" | "double" => Some(Self::F64),
            "f32" | "float" => Some(Self::F32),
            "bool" => Some(Self::Bool),
            "ptr" | "pointer" => Some(Self::Ptr),
            "void" => Some(Self::Void),
            "cstring" => Some(Self::CString),
            "function" | "fn" | "callback" => Some(Self::Function),
            _ => None,
        }
    }

    /// Parse an FFIType from its integer representation.
    pub fn from_u8(n: u8) -> Option<Self> {
        if n <= 17 {
            // Safety: FFIType is repr(u8) with values 0..=17
            Some(unsafe { std::mem::transmute(n) })
        } else {
            None
        }
    }

    /// Size in bytes for this C type.
    pub fn size(self) -> usize {
        match self {
            Self::Char | Self::I8 | Self::U8 | Self::Bool => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 | Self::Ptr | Self::CString
            | Self::Function | Self::I64Fast | Self::U64Fast => 8,
            Self::Void => 0,
        }
    }

    /// Canonical name for this type.
    pub fn name(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F64 => "f64",
            Self::F32 => "f32",
            Self::Bool => "bool",
            Self::Ptr => "ptr",
            Self::Void => "void",
            Self::CString => "cstring",
            Self::I64Fast => "i64_fast",
            Self::U64Fast => "u64_fast",
            Self::Function => "function",
        }
    }

    /// Convert to libffi type descriptor.
    pub fn to_libffi_type(self) -> libffi::middle::Type {
        use libffi::middle::Type;
        match self {
            Self::Void => Type::void(),
            Self::Char | Self::I8 => Type::i8(),
            Self::U8 | Self::Bool => Type::u8(),
            Self::I16 => Type::i16(),
            Self::U16 => Type::u16(),
            Self::I32 => Type::i32(),
            Self::U32 => Type::u32(),
            Self::I64 | Self::I64Fast => Type::i64(),
            Self::U64 | Self::U64Fast => Type::u64(),
            Self::F32 => Type::f32(),
            Self::F64 => Type::f64(),
            Self::Ptr | Self::CString | Self::Function => Type::pointer(),
        }
    }
}

/// Describes the signature of a foreign function.
#[derive(Debug, Clone)]
pub struct FfiSignature {
    pub args: Vec<FFIType>,
    pub returns: FFIType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffi_type_from_name() {
        assert_eq!(FFIType::from_name("i32"), Some(FFIType::I32));
        assert_eq!(FFIType::from_name("int32_t"), Some(FFIType::I32));
        assert_eq!(FFIType::from_name("int"), Some(FFIType::I32));
        assert_eq!(FFIType::from_name("cstring"), Some(FFIType::CString));
        assert_eq!(FFIType::from_name("void"), Some(FFIType::Void));
        assert_eq!(FFIType::from_name("ptr"), Some(FFIType::Ptr));
        assert_eq!(FFIType::from_name("pointer"), Some(FFIType::Ptr));
        assert_eq!(FFIType::from_name("unknown"), None);
    }

    #[test]
    fn test_ffi_type_sizes() {
        assert_eq!(FFIType::I8.size(), 1);
        assert_eq!(FFIType::I16.size(), 2);
        assert_eq!(FFIType::I32.size(), 4);
        assert_eq!(FFIType::I64.size(), 8);
        assert_eq!(FFIType::F64.size(), 8);
        assert_eq!(FFIType::Ptr.size(), 8);
        assert_eq!(FFIType::Void.size(), 0);
    }

    #[test]
    fn test_ffi_type_from_u8() {
        assert_eq!(FFIType::from_u8(5), Some(FFIType::I32));
        assert_eq!(FFIType::from_u8(13), Some(FFIType::Void));
        assert_eq!(FFIType::from_u8(18), None);
        assert_eq!(FFIType::from_u8(255), None);
    }
}
