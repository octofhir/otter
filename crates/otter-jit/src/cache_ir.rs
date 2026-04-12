//! OtterCacheIR — a compact linear bytecode for inline cache operations.
//!
//! Inspired by SpiderMonkey's CacheIR: a single source of truth consumed by
//! the interpreter (for fast paths), baseline JIT (for IC stubs), and the
//! speculative tier (for MIR construction via WarpBuilder-style transpilation).
//!
//! ## Design principles
//!
//! 1. **Code/data separation**: CacheIR contains no pointers. Field references
//!    (Field0, Field1, ...) index into a separate `StubField` array.
//! 2. **Linear**: guards first, then actions, then `ReturnFromIC`.
//! 3. **Compact**: each instruction is 1 byte opcode + inline operands.
//! 4. **Monotonic**: IC state transitions only forward (Uninit → Mono → Poly → Mega).
//!
//! ## Example: monomorphic property load `obj.x`
//!
//! ```text
//! GuardIsObject     input=0
//! GuardShape        obj=0, shape_field=0
//! LoadFixedSlot     obj=0, offset_field=1
//! ReturnFromIC
//! ```
//!
//! The `StubField` array would be:
//! ```text
//! [0] = Shape(ObjectShapeId(42))
//! [1] = Offset(16)
//! ```
//!
//! Spec: Phase 2.2 of JIT_INCREMENTAL_PLAN.md

// ============================================================
// CacheIR opcodes
// ============================================================

/// A single CacheIR instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheIROp {
    // ---- Type guards (fail → next stub in chain) ----

    /// Assert input is an object. Operand: input register index.
    GuardIsObject { input: u8 },
    /// Assert input is Int32.
    GuardIsInt32 { input: u8 },
    /// Assert input is a number (Int32 or Float64).
    GuardIsNumber { input: u8 },
    /// Assert input is a string.
    GuardIsString { input: u8 },
    /// Assert input is a boolean.
    GuardIsBool { input: u8 },
    /// Assert input is undefined.
    GuardIsUndefined { input: u8 },
    /// Assert input is null.
    GuardIsNull { input: u8 },

    // ---- Shape/structure guards ----

    /// Assert object has a specific shape. `shape_field` indexes StubField.
    GuardShape { obj: u8, shape_field: u8 },
    /// Assert object's prototype chain hasn't changed.
    GuardProto { obj: u8, proto_field: u8 },
    /// Assert object is a dense array (no holes, no sparse).
    GuardArrayDense { obj: u8 },
    /// Assert index is within array bounds.
    GuardBoundsCheck { obj: u8, index: u8 },

    // ---- Property loads ----

    /// Load from a fixed slot (inline property). Result stored in output reg.
    LoadFixedSlot { obj: u8, offset_field: u8 },
    /// Load from a dynamic (overflow) slot.
    LoadDynamicSlot { obj: u8, offset_field: u8 },
    /// Load array element by index from dense storage.
    LoadDenseElement { obj: u8, index: u8 },
    /// Load string `.length`.
    LoadStringLength { input: u8 },
    /// Load array `.length`.
    LoadArrayLength { input: u8 },

    // ---- Property stores ----

    /// Store to a fixed slot (inline property).
    StoreFixedSlot { obj: u8, offset_field: u8, val: u8 },
    /// Store to a dynamic slot.
    StoreDynamicSlot { obj: u8, offset_field: u8, val: u8 },
    /// Store element to dense array.
    StoreDenseElement { obj: u8, index: u8, val: u8 },

    // ---- Calls ----

    /// Call a known JS function target. `target_field` indexes StubField.
    CallScriptedFunction { target_field: u8, argc: u8 },
    /// Call a native function.
    CallNativeFunction { target_field: u8, argc: u8 },

    // ---- Megamorphic fallbacks (generic slow paths) ----

    /// Generic property load by name (hash table lookup).
    MegamorphicLoadSlot { obj: u8, name_field: u8 },
    /// Generic property store by name.
    MegamorphicStoreSlot { obj: u8, name_field: u8, val: u8 },

    // ---- Arithmetic fast paths ----

    /// Int32 add with overflow check.
    Int32Add { lhs: u8, rhs: u8 },
    /// Int32 sub with overflow check.
    Int32Sub { lhs: u8, rhs: u8 },
    /// Int32 mul with overflow check.
    Int32Mul { lhs: u8, rhs: u8 },

    // ---- Control ----

    /// Return the result from the IC (success).
    ReturnFromIC,
}

// ============================================================
// StubField — data associated with a CacheIR sequence
// ============================================================

/// A single field in the IC stub's data array.
/// CacheIR instructions reference these by index, not by pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum StubField {
    /// A raw word (u64, used for opaque values).
    RawWord(u64),
    /// An object shape ID.
    Shape(u64),
    /// A slot offset (bytes from object base).
    Offset(u32),
    /// A property name (index into function's property name table).
    Name(u32),
    /// A function target (index into module's function table).
    FunctionTarget(u32),
    /// A raw NaN-boxed value.
    Value(u64),
}

// ============================================================
// CacheIR sequence (one IC stub)
// ============================================================

/// A complete CacheIR sequence for one IC stub.
///
/// Guards first, then actions, then `ReturnFromIC`.
/// If any guard fails, the next stub in the chain is tried.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheIRSequence {
    /// The CacheIR instructions.
    pub ops: Vec<CacheIROp>,
    /// Associated data fields (shapes, offsets, names, etc.).
    pub fields: Vec<StubField>,
}

impl CacheIRSequence {
    /// Create a new empty sequence.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            fields: Vec::new(),
        }
    }

    /// Add a field and return its index.
    pub fn add_field(&mut self, field: StubField) -> u8 {
        let idx = self.fields.len();
        assert!(idx < 256, "too many stub fields");
        self.fields.push(field);
        idx as u8
    }

    /// Add an instruction.
    pub fn push(&mut self, op: CacheIROp) {
        self.ops.push(op);
    }

    /// Build a monomorphic property load CacheIR sequence.
    ///
    /// Equivalent to:
    /// ```text
    /// GuardIsObject   input=0
    /// GuardShape      obj=0, shape_field=F0
    /// LoadFixedSlot   obj=0, offset_field=F1
    /// ReturnFromIC
    /// ```
    #[must_use]
    pub fn monomorphic_prop_load(shape_id: u64, slot_offset: u32) -> Self {
        let mut seq = Self::new();
        let shape_field = seq.add_field(StubField::Shape(shape_id));
        let offset_field = seq.add_field(StubField::Offset(slot_offset));
        seq.push(CacheIROp::GuardIsObject { input: 0 });
        seq.push(CacheIROp::GuardShape { obj: 0, shape_field });
        seq.push(CacheIROp::LoadFixedSlot { obj: 0, offset_field });
        seq.push(CacheIROp::ReturnFromIC);
        seq
    }

    /// Build a monomorphic property store CacheIR sequence.
    #[must_use]
    pub fn monomorphic_prop_store(shape_id: u64, slot_offset: u32) -> Self {
        let mut seq = Self::new();
        let shape_field = seq.add_field(StubField::Shape(shape_id));
        let offset_field = seq.add_field(StubField::Offset(slot_offset));
        seq.push(CacheIROp::GuardIsObject { input: 0 });
        seq.push(CacheIROp::GuardShape { obj: 0, shape_field });
        seq.push(CacheIROp::StoreFixedSlot { obj: 0, offset_field, val: 1 });
        seq.push(CacheIROp::ReturnFromIC);
        seq
    }

    /// Build a dense array element load CacheIR sequence.
    #[must_use]
    pub fn dense_element_load() -> Self {
        let mut seq = Self::new();
        seq.push(CacheIROp::GuardIsObject { input: 0 });
        seq.push(CacheIROp::GuardArrayDense { obj: 0 });
        seq.push(CacheIROp::GuardIsInt32 { input: 1 });
        seq.push(CacheIROp::GuardBoundsCheck { obj: 0, index: 1 });
        seq.push(CacheIROp::LoadDenseElement { obj: 0, index: 1 });
        seq.push(CacheIROp::ReturnFromIC);
        seq
    }

    /// Build an Int32 add CacheIR sequence.
    #[must_use]
    pub fn int32_add() -> Self {
        let mut seq = Self::new();
        seq.push(CacheIROp::GuardIsInt32 { input: 0 });
        seq.push(CacheIROp::GuardIsInt32 { input: 1 });
        seq.push(CacheIROp::Int32Add { lhs: 0, rhs: 1 });
        seq.push(CacheIROp::ReturnFromIC);
        seq
    }
}

impl Default for CacheIRSequence {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// IC Site — manages stub chain for one bytecode site
// ============================================================

/// IC site state machine.
/// `Uninitialized → Monomorphic → Polymorphic → Megamorphic`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ICSiteState {
    /// No IC attached yet.
    Uninitialized,
    /// One stub.
    Monomorphic,
    /// 2-4 stubs.
    Polymorphic,
    /// Too many stubs — use megamorphic fallback.
    Megamorphic,
}

/// Maximum stubs before transitioning to megamorphic.
const MAX_IC_STUBS: usize = 4;

/// An inline cache site: manages the stub chain for one bytecode location.
#[derive(Debug, Clone)]
pub struct ICSite {
    /// Current state.
    pub state: ICSiteState,
    /// Stub chain (most recently added first).
    pub stubs: Vec<CacheIRSequence>,
    /// Bytecode PC this site corresponds to.
    pub bytecode_pc: u32,
}

impl ICSite {
    /// Create a new uninitialized IC site.
    #[must_use]
    pub fn new(bytecode_pc: u32) -> Self {
        Self {
            state: ICSiteState::Uninitialized,
            stubs: Vec::new(),
            bytecode_pc,
        }
    }

    /// Attach a new stub to this IC site.
    pub fn attach_stub(&mut self, stub: CacheIRSequence) {
        match self.state {
            ICSiteState::Megamorphic => return, // Terminal state.
            ICSiteState::Uninitialized => {
                self.stubs.push(stub);
                self.state = ICSiteState::Monomorphic;
            }
            ICSiteState::Monomorphic | ICSiteState::Polymorphic => {
                self.stubs.push(stub);
                if self.stubs.len() > MAX_IC_STUBS {
                    self.state = ICSiteState::Megamorphic;
                    self.stubs.clear(); // No point keeping stubs in mega state.
                } else if self.stubs.len() > 1 {
                    self.state = ICSiteState::Polymorphic;
                }
            }
        }
    }

    /// Whether this site is monomorphic.
    #[must_use]
    pub fn is_monomorphic(&self) -> bool {
        self.state == ICSiteState::Monomorphic
    }

    /// Get the single monomorphic stub, if any.
    #[must_use]
    pub fn monomorphic_stub(&self) -> Option<&CacheIRSequence> {
        if self.is_monomorphic() {
            self.stubs.first()
        } else {
            None
        }
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monomorphic_prop_load() {
        let seq = CacheIRSequence::monomorphic_prop_load(42, 16);
        assert_eq!(seq.ops.len(), 4);
        assert!(matches!(seq.ops[0], CacheIROp::GuardIsObject { input: 0 }));
        assert!(matches!(seq.ops[1], CacheIROp::GuardShape { obj: 0, .. }));
        assert!(matches!(seq.ops[2], CacheIROp::LoadFixedSlot { obj: 0, .. }));
        assert!(matches!(seq.ops[3], CacheIROp::ReturnFromIC));
        assert_eq!(seq.fields.len(), 2);
        assert_eq!(seq.fields[0], StubField::Shape(42));
        assert_eq!(seq.fields[1], StubField::Offset(16));
    }

    #[test]
    fn test_ic_site_transitions() {
        let mut site = ICSite::new(0);
        assert_eq!(site.state, ICSiteState::Uninitialized);

        site.attach_stub(CacheIRSequence::monomorphic_prop_load(1, 0));
        assert_eq!(site.state, ICSiteState::Monomorphic);
        assert!(site.is_monomorphic());

        site.attach_stub(CacheIRSequence::monomorphic_prop_load(2, 8));
        assert_eq!(site.state, ICSiteState::Polymorphic);

        site.attach_stub(CacheIRSequence::monomorphic_prop_load(3, 16));
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(4, 24));
        assert_eq!(site.state, ICSiteState::Polymorphic);

        // 5th stub → megamorphic
        site.attach_stub(CacheIRSequence::monomorphic_prop_load(5, 32));
        assert_eq!(site.state, ICSiteState::Megamorphic);
        assert!(site.stubs.is_empty()); // Stubs cleared in mega state.
    }

    #[test]
    fn test_int32_add_sequence() {
        let seq = CacheIRSequence::int32_add();
        assert_eq!(seq.ops.len(), 4);
        assert!(matches!(seq.ops[0], CacheIROp::GuardIsInt32 { input: 0 }));
        assert!(matches!(seq.ops[1], CacheIROp::GuardIsInt32 { input: 1 }));
        assert!(matches!(seq.ops[2], CacheIROp::Int32Add { lhs: 0, rhs: 1 }));
    }

    #[test]
    fn test_dense_element_load() {
        let seq = CacheIRSequence::dense_element_load();
        assert_eq!(seq.ops.len(), 6);
    }
}
