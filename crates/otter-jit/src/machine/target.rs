//! Physical register inventories for supported machine targets.
//!
//! # Contents
//! - [`TargetRegisterFile`] — complete allocatable, non-preferred, and scratch
//!   register sets consumed by register allocation.
//! - [`TargetArchitecture`] — supported instruction-selector targets.
//!
//! # Invariants
//! - Scratch registers are not allocatable.
//! - Integer and floating-point register encodings use the target's hardware
//!   numbering and remain in disjoint regalloc2 classes.
//! - Platform-reserved stack/frame/link registers are never allocatable.

use regalloc2::{MachineEnv, PReg, PRegSet, RegClass};

/// Architecture selected before register allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArchitecture {
    /// AArch64 Procedure Call Standard register file.
    Aarch64,
    /// System V x86-64 register file.
    X86_64,
}

/// One target physical register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalRegister {
    class: RegClass,
    encoding: u8,
}

impl PhysicalRegister {
    /// Construct an integer-class physical register.
    #[must_use]
    pub const fn integer(encoding: u8) -> Self {
        Self {
            class: RegClass::Int,
            encoding,
        }
    }

    /// Construct a floating-point physical register.
    #[must_use]
    pub const fn float(encoding: u8) -> Self {
        Self {
            class: RegClass::Float,
            encoding,
        }
    }

    /// Hardware register encoding.
    #[must_use]
    pub const fn encoding(self) -> u8 {
        self.encoding
    }

    /// Whether this is an integer-class register.
    #[must_use]
    pub const fn is_integer(self) -> bool {
        matches!(self.class, RegClass::Int)
    }

    /// Whether this is a floating-point register.
    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self.class, RegClass::Float)
    }

    pub(super) const fn class(self) -> RegClass {
        self.class
    }

    pub(super) fn as_regalloc(self) -> PReg {
        PReg::new(usize::from(self.encoding), self.class)
    }
}

/// Explicit physical register inventory for one target.
#[derive(Debug, Clone)]
pub struct TargetRegisterFile {
    architecture: TargetArchitecture,
    preferred: [Vec<PhysicalRegister>; 3],
    non_preferred: [Vec<PhysicalRegister>; 3],
    scratch: [Option<PhysicalRegister>; 3],
}

impl TargetRegisterFile {
    /// Complete AArch64 register inventory.
    #[must_use]
    pub fn aarch64() -> Self {
        // x16/x17 are intra-procedure-call scratch, x18 is platform-reserved,
        // x29/x30 are frame/link, and x31 is SP/ZR. v31 is the FP move scratch.
        Self {
            architecture: TargetArchitecture::Aarch64,
            preferred: [
                (0..=15).map(PhysicalRegister::integer).collect(),
                (0..=15).map(PhysicalRegister::float).collect(),
                Vec::new(),
            ],
            non_preferred: [
                (19..=28).map(PhysicalRegister::integer).collect(),
                (16..=30).map(PhysicalRegister::float).collect(),
                Vec::new(),
            ],
            scratch: [
                Some(PhysicalRegister::integer(16)),
                Some(PhysicalRegister::float(31)),
                None,
            ],
        }
    }

    pub(super) fn aarch64_scalar_function() -> Self {
        let mut registers = Self::aarch64();
        // x19 retains the context, x15..x18 are emitter/platform scratch, and
        // v16..v31 stay outside the scalar contract. Values live across leaf
        // calls may use the remaining AAPCS64 callee-saved file; the scalar
        // emitter derives its exact save set and deopt dump from this one
        // inventory.
        registers.preferred[RegClass::Int as usize].retain(|register| register.encoding != 15);
        registers.non_preferred[RegClass::Int as usize]
            .retain(|register| (20..=28).contains(&register.encoding));
        registers.preferred[RegClass::Float as usize].retain(|register| register.encoding <= 7);
        registers.non_preferred[RegClass::Float as usize]
            .retain(|register| (8..=15).contains(&register.encoding));
        registers
    }

    /// Caller-saved registers exposed to scalar-function allocation.
    pub(super) fn aarch64_scalar_call_clobbers() -> Vec<PhysicalRegister> {
        (0..=14)
            .map(PhysicalRegister::integer)
            .chain((0..=7).map(PhysicalRegister::float))
            .collect()
    }

    /// Complete System V x86-64 register inventory.
    #[must_use]
    pub fn x86_64() -> Self {
        // rsp/rbp are frame-owned and r11 is the integer move scratch. xmm15
        // is the floating-point move scratch.
        Self {
            architecture: TargetArchitecture::X86_64,
            preferred: [
                [0, 1, 2, 6, 7, 8, 9, 10]
                    .into_iter()
                    .map(PhysicalRegister::integer)
                    .collect(),
                (0..=7).map(PhysicalRegister::float).collect(),
                Vec::new(),
            ],
            non_preferred: [
                [3, 12, 13, 14, 15]
                    .into_iter()
                    .map(PhysicalRegister::integer)
                    .collect(),
                (8..=14).map(PhysicalRegister::float).collect(),
                Vec::new(),
            ],
            scratch: [
                Some(PhysicalRegister::integer(11)),
                Some(PhysicalRegister::float(15)),
                None,
            ],
        }
    }

    /// Architecture owning this register file.
    #[must_use]
    pub const fn architecture(&self) -> TargetArchitecture {
        self.architecture
    }

    pub(super) fn environment(&self) -> MachineEnv {
        let mut preferred = [PRegSet::empty(); 3];
        let mut non_preferred = [PRegSet::empty(); 3];
        for class in 0..3 {
            for &register in &self.preferred[class] {
                preferred[class].add(register.as_regalloc());
            }
            for &register in &self.non_preferred[class] {
                non_preferred[class].add(register.as_regalloc());
            }
        }
        MachineEnv {
            preferred_regs_by_class: preferred,
            non_preferred_regs_by_class: non_preferred,
            scratch_by_class: self
                .scratch
                .map(|register| register.map(PhysicalRegister::as_regalloc)),
            fixed_stack_slots: Vec::new(),
        }
    }
}
