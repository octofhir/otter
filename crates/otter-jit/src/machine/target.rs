//! Immutable target facts consumed by Machine selection and allocation.
//!
//! # Contents
//! - [`TargetSpec`] — the one compiler input owning ABI, frame, register, and
//!   legalization facts for a target.
//! - [`TargetRegisterFile`] — complete allocatable, non-preferred, and scratch
//!   register sets consumed by register allocation.
//! - [`TargetArchitecture`] — supported instruction-selector targets.
//!
//! # Invariants
//! - Scratch registers are not allocatable.
//! - Integer and floating-point register encodings use the target's hardware
//!   numbering and remain in disjoint regalloc2 classes.
//! - Platform-reserved stack/frame/link registers are never allocatable.
//! - Neutral Machine code obtains fixed registers, clobbers, frame limits, and
//!   legalization availability only through an explicit [`TargetSpec`].
//!
//! # See also
//! - [`crate::machine::regalloc`] — the allocator consumer of target facts.
//! - [`crate::machine::numeric`] — target-neutral Machine selection.

use regalloc2::{MachineEnv, PReg, PRegSet, RegClass};

use super::{AllocatedSequence, FrameLayoutError, MachineFrameLayout};

pub(super) const AARCH64_DEOPT_GPR_BUDGET: u16 = 30;
pub(super) const AARCH64_DEOPT_FP_BUDGET: u16 = 16;
pub(super) const AARCH64_BASE_FIXED_FRAME_BYTES: u32 = 32;

/// Architecture selected before register allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArchitecture {
    /// AArch64 Procedure Call Standard register file.
    Aarch64,
    /// System V x86-64 register file.
    X86_64,
}

/// Closed clobber families used by target-neutral Machine operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TargetClobberSet {
    /// Ordinary scalar runtime call.
    ScalarCall,
    /// Guarded element access.
    Element,
    /// Named-property load probe.
    PropertyLoad,
    /// Named-property store probe.
    PropertyStore,
    /// Binding proof.
    BindingGuard,
    /// Binding generated hit.
    BindingHit,
    /// Binding write barrier.
    BindingWriteBarrier,
    /// Stable string-cell load.
    StringConstantLoad,
    /// Tagged nullish test and native-status branch.
    StatusScratch,
    /// Inlined method identity/prototype proof.
    InlineMethodGuard,
    /// Inlined plain/construct call identity proof.
    InlineCallGuard,
    /// Base-constructor receiver allocation probe.
    ConstructReceiver,
    /// Receiver-allocation hit test.
    ConstructReceiverHit,
    /// Base-constructor result selection.
    BaseConstructResult,
    /// Float64-to-element-index legalization.
    FloatElementIndex,
    /// Proven Int32 native leaf.
    NativeLeafInt32,
}

/// Target legalization features admitted by neutral instruction selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TargetCapability {
    /// Fixed-register Float64-to-Int32 conversion call.
    Float64ToInt32,
    /// Fixed-register floating remainder call.
    FloatRemainder,
    /// Fixed-register floating exponentiation call.
    FloatPower,
    /// Guarded static-native leaf call.
    NativeLeaf,
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
    fn aarch64() -> Self {
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

    fn aarch64_scalar_function() -> Self {
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

    /// Complete System V x86-64 register inventory.
    fn x86_64_scalar_function() -> Self {
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
                [3, 12, 13, 14]
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

#[derive(Debug, Clone)]
struct TargetCallConvention {
    integer_arguments: Box<[PhysicalRegister]>,
    float_arguments: Box<[PhysicalRegister]>,
    integer_result: PhysicalRegister,
    float_result: PhysicalRegister,
    callee: PhysicalRegister,
    context: PhysicalRegister,
}

#[derive(Debug, Clone)]
struct TargetFrameSpec {
    base_fixed_bytes: u32,
    stack_alignment: u32,
    entry_stack_bias: u32,
    deopt_gpr_budget: u16,
    deopt_fp_budget: u16,
    saved_gpr_first: u8,
    saved_gpr_last: u8,
    saved_gpr_implicit: u8,
    extra_saved_gprs: Box<[u8]>,
    saved_fp_first: u8,
    saved_fp_last: u8,
}

/// One immutable set of target facts for the complete Machine pipeline.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    architecture: TargetArchitecture,
    registers: TargetRegisterFile,
    calls: TargetCallConvention,
    frame: TargetFrameSpec,
    clobbers: [Box<[PhysicalRegister]>; 16],
    capabilities: [bool; 4],
}

impl TargetSpec {
    /// AArch64 scalar Machine contract.
    #[must_use]
    pub fn aarch64() -> Self {
        let integer = PhysicalRegister::integer;
        let float = PhysicalRegister::float;
        let scalar_call = (0..=14)
            .map(integer)
            .chain((0..=7).map(float))
            .collect::<Box<[_]>>();
        Self {
            architecture: TargetArchitecture::Aarch64,
            registers: TargetRegisterFile::aarch64_scalar_function(),
            calls: TargetCallConvention {
                integer_arguments: (0..=8).map(integer).collect(),
                float_arguments: (0..=7).map(float).collect(),
                integer_result: integer(0),
                float_result: float(0),
                callee: integer(9),
                context: integer(19),
            },
            frame: TargetFrameSpec {
                base_fixed_bytes: AARCH64_BASE_FIXED_FRAME_BYTES,
                stack_alignment: 16,
                entry_stack_bias: 0,
                deopt_gpr_budget: AARCH64_DEOPT_GPR_BUDGET,
                deopt_fp_budget: AARCH64_DEOPT_FP_BUDGET,
                saved_gpr_first: 20,
                saved_gpr_last: 28,
                saved_gpr_implicit: 1,
                extra_saved_gprs: Box::new([]),
                saved_fp_first: 8,
                saved_fp_last: 15,
            },
            clobbers: [
                scalar_call.clone(),
                (9..=16)
                    .map(integer)
                    .chain([float(30), float(31)])
                    .collect(),
                (9..=16).map(integer).collect(),
                scalar_call,
                (9..=16).map(integer).collect(),
                vec![integer(9)].into_boxed_slice(),
                [0, 1, 2, 9, 11, 12, 14, 15, 16].map(integer).into(),
                [9, 13].map(integer).into(),
                vec![integer(16)].into_boxed_slice(),
                (9..=15).map(integer).collect(),
                [9, 10, 11, 12, 14].map(integer).into(),
                [0, 1, 2, 4, 9, 10, 11, 12, 13, 14, 15, 16]
                    .map(integer)
                    .into(),
                [9, 10].map(integer).into(),
                [0, 9, 10, 11].map(integer).into(),
                [integer(16), float(31)].into(),
                (12..=14).map(integer).collect(),
            ],
            capabilities: [true; 4],
        }
    }

    /// System V x86-64 contract usable by neutral verifier/regalloc tests.
    #[must_use]
    pub fn x86_64() -> Self {
        let integer = PhysicalRegister::integer;
        let float = PhysicalRegister::float;
        let scalar_call = [0, 1, 2, 6, 7, 8, 9, 10, 11]
            .map(integer)
            .into_iter()
            .chain((0..=15).map(float))
            .collect::<Box<[_]>>();
        Self {
            architecture: TargetArchitecture::X86_64,
            registers: TargetRegisterFile::x86_64_scalar_function(),
            calls: TargetCallConvention {
                integer_arguments: [7, 6, 2, 1, 8, 9].map(integer).into(),
                float_arguments: (0..=7).map(float).collect(),
                integer_result: integer(0),
                float_result: float(0),
                callee: integer(10),
                context: integer(15),
            },
            frame: TargetFrameSpec {
                // `rbp` and the retained `r15` context are always pushed.
                base_fixed_bytes: 16,
                stack_alignment: 16,
                // System V enters after an eight-byte return-address push.
                entry_stack_bias: 8,
                deopt_gpr_budget: 16,
                deopt_fp_budget: 16,
                saved_gpr_first: 12,
                saved_gpr_last: 14,
                saved_gpr_implicit: 0,
                extra_saved_gprs: Box::new([3]),
                saved_fp_first: 16,
                saved_fp_last: 15,
            },
            // Until the selector/emitter proves narrower scratch contracts,
            // every neutral operation conservatively reserves the complete
            // System V caller-saved set. This is safe input to verifier and
            // regalloc tests, not a claim of implemented x86 lowering.
            clobbers: std::array::from_fn(|_| scalar_call.clone()),
            capabilities: [true; 4],
        }
    }

    /// Target architecture selected before allocation.
    #[must_use]
    pub const fn architecture(&self) -> TargetArchitecture {
        self.architecture
    }

    /// Allocatable register inventory.
    #[must_use]
    pub const fn registers(&self) -> &TargetRegisterFile {
        &self.registers
    }

    /// One ABI integer argument register.
    #[must_use]
    pub fn integer_argument(&self, index: usize) -> Option<PhysicalRegister> {
        self.calls.integer_arguments.get(index).copied()
    }

    /// One ABI floating-point argument register.
    #[must_use]
    pub fn float_argument(&self, index: usize) -> Option<PhysicalRegister> {
        self.calls.float_arguments.get(index).copied()
    }

    /// ABI integer result register.
    #[must_use]
    pub const fn integer_result(&self) -> PhysicalRegister {
        self.calls.integer_result
    }

    /// ABI floating-point result register.
    #[must_use]
    pub const fn float_result(&self) -> PhysicalRegister {
        self.calls.float_result
    }

    /// Fixed register containing a static-native leaf callable.
    #[must_use]
    pub const fn callee_register(&self) -> PhysicalRegister {
        self.calls.callee
    }

    /// Fixed register retaining the runtime context.
    #[must_use]
    pub const fn context_register(&self) -> PhysicalRegister {
        self.calls.context
    }

    /// Exact clobber set for one neutral Machine operation family.
    #[must_use]
    pub fn clobbers(&self, set: TargetClobberSet) -> &[PhysicalRegister] {
        &self.clobbers[set as usize]
    }

    /// Whether target lowering implements a Machine legalization feature.
    #[must_use]
    pub const fn supports(&self, capability: TargetCapability) -> bool {
        self.capabilities[capability as usize]
    }

    /// Register count used by the deoptimization physical namespace.
    #[must_use]
    pub const fn deopt_register_budgets(&self) -> (u16, u16) {
        (self.frame.deopt_gpr_budget, self.frame.deopt_fp_budget)
    }

    /// Whether a register contributes target callee-saved frame state.
    #[must_use]
    pub fn is_callee_saved(&self, register: PhysicalRegister) -> bool {
        (register.is_integer()
            && (self.frame.saved_gpr_first..=self.frame.saved_gpr_last)
                .contains(&register.encoding()))
            || (register.is_integer() && self.frame.extra_saved_gprs.contains(&register.encoding()))
            || (register.is_float()
                && (self.frame.saved_fp_first..=self.frame.saved_fp_last)
                    .contains(&register.encoding()))
    }

    /// Build the target-owned native frame around one allocation result.
    pub fn frame_layout(
        &self,
        allocation: &AllocatedSequence,
        root_slots: u16,
        raw_slots: u16,
    ) -> Result<MachineFrameLayout, FrameLayoutError> {
        if allocation.architecture() != self.architecture {
            return Err(FrameLayoutError::TargetMismatch);
        }
        let highest_gpr = allocation
            .used_registers()
            .filter(|register| {
                register.is_integer()
                    && (self.frame.saved_gpr_first..=self.frame.saved_gpr_last)
                        .contains(&register.encoding())
            })
            .map(PhysicalRegister::encoding)
            .max();
        let highest_fp = allocation
            .used_registers()
            .filter(|register| register.is_float() && self.is_callee_saved(*register))
            .map(PhysicalRegister::encoding)
            .max();
        let gpr_count = highest_gpr
            .map(|register| register - self.frame.saved_gpr_first + 1)
            .unwrap_or(0);
        let fp_count = highest_fp
            .map(|register| register - self.frame.saved_fp_first + 1)
            .unwrap_or(0);
        let extra_gpr_count = allocation
            .used_registers()
            .filter(|register| {
                register.is_integer() && self.frame.extra_saved_gprs.contains(&register.encoding())
            })
            .count() as u32;
        let fixed_bytes = self.frame.base_fixed_bytes
            + u32::from(gpr_count.saturating_sub(self.frame.saved_gpr_implicit))
                .saturating_mul(8)
                .next_multiple_of(16)
            + extra_gpr_count.saturating_mul(8)
            + u32::from(fp_count).saturating_mul(8).next_multiple_of(16);
        MachineFrameLayout::new_with_target_alignment(
            allocation,
            root_slots,
            raw_slots,
            fixed_bytes,
            self.frame.stack_alignment,
            self.frame.entry_stack_bias,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_targets_publish_complete_machine_contracts() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            assert_eq!(target.architecture(), target.registers().architecture());
            assert!(target.integer_argument(0).is_some());
            assert!(target.integer_argument(1).is_some());
            assert!(target.float_argument(0).is_some());
            assert!(
                !target
                    .clobbers(TargetClobberSet::ScalarCall)
                    .contains(&target.context_register())
            );
            assert_ne!(target.callee_register(), target.context_register());
            let (gpr, fp) = target.deopt_register_budgets();
            assert!(gpr > 0 && fp > 0);
        }
    }

    #[test]
    fn receiver_candidate_results_are_declared_clobbers() {
        let target = TargetSpec::aarch64();
        let clobbers = target.clobbers(TargetClobberSet::ConstructReceiver);
        assert!(clobbers.contains(&PhysicalRegister::integer(0)));
        assert!(clobbers.contains(&PhysicalRegister::integer(1)));
    }

    #[test]
    fn element_number_canonicalization_declares_every_scratch() {
        let target = TargetSpec::aarch64();
        let clobbers = target.clobbers(TargetClobberSet::Element);
        for register in 9..=16 {
            assert!(clobbers.contains(&PhysicalRegister::integer(register)));
        }
        assert!(clobbers.contains(&PhysicalRegister::float(30)));
        assert!(clobbers.contains(&PhysicalRegister::float(31)));
    }

    #[test]
    fn neutral_machine_sources_do_not_construct_an_architecture() {
        let sources = [
            ("machine/mod.rs", include_str!("mod.rs")),
            ("machine/deopt.rs", include_str!("deopt.rs")),
            ("machine/frame.rs", include_str!("frame.rs")),
            ("machine/native_leaf.rs", include_str!("native_leaf.rs")),
            ("machine/regalloc.rs", include_str!("regalloc.rs")),
            ("machine/safepoint.rs", include_str!("safepoint.rs")),
            ("machine/numeric/mod.rs", include_str!("numeric/mod.rs")),
            ("machine/numeric/hir.rs", include_str!("numeric/hir.rs")),
            (
                "machine/numeric/inline_reentry.rs",
                include_str!("numeric/inline_reentry.rs"),
            ),
            (
                "machine/numeric/property_cfg.rs",
                include_str!("numeric/property_cfg.rs"),
            ),
        ];
        let forbidden = [
            "TargetSpec::aarch64",
            "TargetSpec::x86_64",
            "aarch64_scalar_",
            "x86_64_scalar_",
        ];
        for (path, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for needle in forbidden {
                assert!(
                    !production.contains(needle),
                    "{path} chooses a target through {needle}"
                );
            }
        }
    }
}
