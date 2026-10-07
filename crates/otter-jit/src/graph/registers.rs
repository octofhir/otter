//! Physical register ownership supplied to the graph allocator by its backend.
//!
//! # Contents
//! - [`RegisterContract`] describes allocation pools, calls and arithmetic words.
//! - [`IntDivisionRegisters`] names the quotient and remainder registers.
//! - [`ReceiverAllocationRegisters`] names inline constructor receiver words.
//! - `AARCH64` / `X86_64` supply each native Graph instruction encoder.
//!
//! # Invariants
//! - Every physical index fits the allocator's 32-bit register masks.
//! - Pinned and emitter scratch registers never belong to an allocation pool.
//! - Fixed inputs, outputs and arithmetic clobbers belong to the allocation pool.
//! - Every encoder consumes this contract for input, output and scratch ownership.
//!
//! # See also
//! - [`super::regalloc`] for canonical homes and physical assignment.
//! - [`super::ir::Kind::constraints`] for per-operation register requirements.

/// Implicit integer-division outputs and clobbers supplied by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntDivisionRegisters {
    pub(crate) quotient: u8,
    pub(crate) remainder: u8,
}

/// The fixed words of an inline constructor receiver allocation: the target
/// encoder proves new.target in `new_target`, leaves the receiver in
/// `result` and writes every register in `clobbers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceiverAllocationRegisters {
    pub(crate) new_target: u8,
    pub(crate) result: u8,
    pub(crate) clobbers: &'static [u8],
}

/// Immutable backend register, call-ABI and arithmetic ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegisterContract {
    pub(crate) general: &'static [u8],
    pub(crate) floating: &'static [u8],
    pub(crate) pinned_general: &'static [u8],
    pub(crate) scratch_general: &'static [u8],
    pub(crate) scratch_floating: &'static [u8],
    pub(crate) call_callee: u8,
    pub(crate) call_receiver: u8,
    pub(crate) call_result: u8,
    /// A variable shift reads the low count byte of this general register.
    /// `None` means the backend accepts an ordinary register operand.
    pub(crate) variable_shift_count: Option<u8>,
    /// `None` means division has ordinary inputs, output and temporaries.
    pub(crate) integer_division: Option<IntDivisionRegisters>,
    pub(crate) receiver_allocation: ReceiverAllocationRegisters,
}

impl RegisterContract {
    pub(crate) fn validate(self) {
        fn mask(registers: &[u8]) -> u32 {
            let mut mask = 0;
            for &register in registers {
                assert!(register < 32, "physical register exceeds allocator mask");
                let bit = 1u32 << register;
                assert_eq!(mask & bit, 0, "duplicate physical register");
                mask |= bit;
            }
            mask
        }
        let general = mask(self.general);
        let floating = mask(self.floating);
        let pinned = mask(self.pinned_general);
        let scratch = mask(self.scratch_general);
        assert_ne!(general, 0, "a backend requires general registers");
        assert_ne!(floating, 0, "a backend requires floating registers");
        assert_eq!(
            general & (pinned | scratch),
            0,
            "reserved general register is allocatable"
        );
        assert_eq!(
            pinned & scratch,
            0,
            "pinned register cannot be emitter scratch"
        );
        assert_eq!(
            floating & mask(self.scratch_floating),
            0,
            "reserved floating register is allocatable"
        );
        for register in [self.call_callee, self.call_receiver, self.call_result] {
            assert!(
                register < 32 && general & (1 << register) != 0,
                "fixed call word must have an allocatable physical register"
            );
        }
        for register in self.variable_shift_count.into_iter().chain(
            self.integer_division
                .into_iter()
                .flat_map(|pair| [pair.quotient, pair.remainder]),
        ) {
            assert!(
                register < 32 && general & (1 << register) != 0,
                "fixed arithmetic word must have an allocatable physical register"
            );
        }
        let receiver = self.receiver_allocation;
        for register in receiver
            .clobbers
            .iter()
            .copied()
            .chain([receiver.new_target, receiver.result])
        {
            assert!(
                register < 32 && general & (1 << register) != 0,
                "fixed receiver word must have an allocatable physical register"
            );
        }
        assert!(
            !receiver.clobbers.contains(&receiver.new_target)
                && receiver.new_target != receiver.result,
            "receiver allocation preserves its new.target input"
        );
        if let Some(pair) = self.integer_division {
            assert_ne!(
                pair.quotient, pair.remainder,
                "quotient and remainder require distinct physical registers"
            );
        }
    }
}

#[cfg(any(test, target_arch = "aarch64"))]
pub(crate) const AARCH64: RegisterContract = RegisterContract {
    // x22–x28 are C callee-saved: a body saves the pairs it uses below its
    // frame pointer. Every allocatable register is clobbered by calls.
    general: &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 22, 23, 24, 25, 26, 27, 28,
    ],
    floating: &[
        0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
    ],
    pinned_general: &[19, 20, 21, 29, 30, 31],
    scratch_general: &[16, 17],
    scratch_floating: &[31],
    call_callee: 1,
    call_receiver: 2,
    call_result: 0,
    variable_shift_count: None,
    integer_division: None,
    receiver_allocation: ReceiverAllocationRegisters {
        new_target: 2,
        result: 0,
        clobbers: &[1, 4, 11, 12, 13, 14, 15],
    },
};

#[cfg(any(test, target_arch = "x86_64"))]
pub(crate) const X86_64: RegisterContract = RegisterContract {
    general: &[0, 1, 2, 3, 6, 7, 8, 9, 12],
    floating: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
    pinned_general: &[4, 5, 13, 14, 15],
    scratch_general: &[10, 11],
    scratch_floating: &[15],
    call_callee: 6,
    call_receiver: 2,
    call_result: 0,
    variable_shift_count: Some(1),
    integer_division: Some(IntDivisionRegisters {
        quotient: 0,
        remainder: 2,
    }),
    receiver_allocation: ReceiverAllocationRegisters {
        new_target: 1,
        result: 0,
        clobbers: &[2, 7, 8, 9],
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_exclude_reserved_registers_and_describe_distinct_call_abis() {
        AARCH64.validate();
        X86_64.validate();
        assert_ne!(AARCH64.call_callee, X86_64.call_callee);
        assert_eq!(X86_64.general.len(), 9);
        assert_eq!(X86_64.floating.len(), 15);
    }

    #[test]
    #[should_panic(expected = "reserved general register is allocatable")]
    fn contract_rejects_a_pinned_register_in_the_allocator_pool() {
        RegisterContract {
            pinned_general: &[0],
            ..AARCH64
        }
        .validate();
    }

    #[test]
    #[should_panic(expected = "fixed arithmetic word must have an allocatable physical register")]
    fn contract_rejects_a_scratch_shift_count() {
        RegisterContract {
            variable_shift_count: Some(10),
            ..X86_64
        }
        .validate();
    }

    #[test]
    #[should_panic(expected = "quotient and remainder require distinct physical registers")]
    fn contract_rejects_overlapping_division_outputs() {
        RegisterContract {
            integer_division: Some(IntDivisionRegisters {
                quotient: 0,
                remainder: 0,
            }),
            ..X86_64
        }
        .validate();
    }
}
