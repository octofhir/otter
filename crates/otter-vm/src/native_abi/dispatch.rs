//! One fixed native result contract for compiled code and runtime stubs.
//!
//! # Contents
//! - [`NativeResultPair`] is the sole two-register result carrier.
//! - [`NativeResultStatus`] is the sole machine-observed status alphabet.
//! - [`SideExit`], [`ExitReason`], and [`ExitAction`] are the typed pre-effect
//!   exit payload shared by every compiled tier.
//! - [`NativeResultDomain`] validates the status subset and payload semantics
//!   owned by each native boundary.
//!
//! # Invariants
//! - The pair is always exactly `x0 = payload_bits`, `x1 = status`; typed side
//!   exits pack PC/reason/action into that one payload rather than introducing
//!   another result carrier.
//! - JavaScript exceptions never unwind through native frames. Compiled,
//!   structured-exception, and committed domains carry a pure boxed exception
//!   in `x0`; the probe domain reports a pending exception with canonical zero
//!   payload instead.
//! - A compiled or structured-exception [`NativeResultStatus::SideExit`]
//!   payload is one valid [`SideExit`]. A probe-domain side exit is a guard miss
//!   and has canonical zero payload.
//! - Every Rust consumer validates the externally known domain before reading
//!   status-specific payload semantics. Unknown words and statuses outside the
//!   selected domain are rejected, never silently rewritten as `Fatal`.
//! - Records are fixed-width C-layout values suitable for two-register returns.
//!
//! # See also
//! - [`super::runtime_stubs`] for descriptor-side result-domain ownership.

/// Why generated execution left before committing its source operation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExitReason {
    /// A value did not satisfy a proven representation or semantic type.
    TypeMismatch = 1,
    /// Checked Int32 arithmetic overflowed.
    Int32Overflow = 2,
    /// An Int32 result would erase an observable negative zero.
    NegativeZero = 3,
    /// An indexed access could not prove an exact ECMAScript array index.
    InvalidElementIndex = 4,
    /// A callable, receiver, or constructor identity proof failed.
    IdentityGuard = 5,
    /// An object/prototype shape proof failed.
    ShapeGuard = 6,
    /// A length or storage-bounds proof failed.
    BoundsGuard = 7,
    /// Interrupt or work-budget polling requested interpreter control.
    Interrupt = 8,
    /// A pre-effect generated allocation probe missed.
    AllocationMiss = 9,
    /// The baseline tier reached an operation it deliberately did not emit.
    UnsupportedOperation = 10,
    /// A VM transition selected interpreter continuation without a failed speculation.
    RuntimeTransition = 11,
}

impl ExitReason {
    const fn decode(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::TypeMismatch),
            2 => Some(Self::Int32Overflow),
            3 => Some(Self::NegativeZero),
            4 => Some(Self::InvalidElementIndex),
            5 => Some(Self::IdentityGuard),
            6 => Some(Self::ShapeGuard),
            7 => Some(Self::BoundsGuard),
            8 => Some(Self::Interrupt),
            9 => Some(Self::AllocationMiss),
            10 => Some(Self::UnsupportedOperation),
            11 => Some(Self::RuntimeTransition),
            _ => None,
        }
    }
}

/// Cold policy requested by one typed side-exit site.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExitAction {
    /// Resume without changing compilation policy.
    Resume = 0,
    /// Feed the reason/site profile into a later optimizing compilation.
    Recompile = 1,
    /// Invalidate the owning generation immediately.
    Invalidate = 2,
}

impl ExitAction {
    const fn decode(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Resume),
            1 => Some(Self::Recompile),
            2 => Some(Self::Invalidate),
            _ => None,
        }
    }
}

/// Exact typed payload returned with [`NativeResultStatus::SideExit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideExit {
    logical_pc: u32,
    reason: ExitReason,
    action: ExitAction,
}

impl SideExit {
    const REASON_SHIFT: u32 = 32;
    const ACTION_SHIFT: u32 = 40;
    const USED_MASK: u64 =
        u32::MAX as u64 | (u8::MAX as u64) << Self::REASON_SHIFT | 0x3 << Self::ACTION_SHIFT;

    /// Construct one pre-effect exit contract.
    #[must_use]
    pub const fn new(logical_pc: u32, reason: ExitReason, action: ExitAction) -> Self {
        Self {
            logical_pc,
            reason,
            action,
        }
    }

    /// Exact interpreter instruction-index PC.
    #[must_use]
    pub const fn logical_pc(self) -> u32 {
        self.logical_pc
    }

    /// Stable reason identity used by exit profiling.
    #[must_use]
    pub const fn reason(self) -> ExitReason {
        self.reason
    }

    /// Policy action requested by the generated site.
    #[must_use]
    pub const fn action(self) -> ExitAction {
        self.action
    }

    /// Encode into the one native result payload word.
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.logical_pc as u64
            | (self.reason as u64) << Self::REASON_SHIFT
            | (self.action as u64) << Self::ACTION_SHIFT
    }

    /// Decode one exact payload, rejecting spare bits and unknown enum values.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::USED_MASK != 0 {
            return None;
        }
        let reason = match ExitReason::decode((bits >> Self::REASON_SHIFT) as u8) {
            Some(reason) => reason,
            None => return None,
        };
        let action = match ExitAction::decode((bits >> Self::ACTION_SHIFT) as u8 & 0x3) {
            Some(action) => action,
            None => return None,
        };
        Some(Self {
            logical_pc: bits as u32,
            reason,
            action,
        })
    }
}

/// The one machine-observed native result status alphabet.
///
/// Domains deliberately give shared status words their local meaning:
/// `SideExit` is a compiled/exception `Bail` or a probe `Miss`, while
/// `Success` is a compiled `Return` or a runtime-stub `Ok`.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeResultStatus {
    /// The selected native operation completed normally.
    Success = 0,
    /// Leave the selected generated/fast path before its source effect.
    SideExit = 1,
    /// JavaScript abrupt completion.
    Throw = 2,
    /// A structured exception transition committed and generated fallthrough
    /// remains authoritative.
    Continue = 3,
    /// A probe/allocation boundary could not allocate.
    OutOfMemory = 4,
    /// Structural engine failure parked in the active runtime context.
    Fatal = 6,
}

/// Semantic owner of one [`NativeResultPair`].
///
/// `None` is a descriptor sentinel for machine signatures that return a word,
/// raw value, or float instead of a pair; it is never a valid pair domain.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeResultDomain {
    /// This descriptor does not return a [`NativeResultPair`].
    None = 0,
    /// Whole compiled-function entry/exit.
    Compiled = 1,
    /// Structured exception-region transition.
    ExceptionTransition = 2,
    /// Effect-once JavaScript semantic boundary.
    Committed = 3,
    /// Pre-effect leaf/allocation probe.
    Probe = 4,
}

/// The sole fixed two-register native result.
///
/// The domain is owned by the compiled-entry signature or runtime-stub
/// descriptor, not duplicated in this carrier. This keeps every machine call
/// on one physical ABI while [`Self::validate`] prevents a caller from
/// interpreting a status outside that boundary's state machine.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeResultPair {
    payload_bits: u64,
    status: u64,
}

impl NativeResultPair {
    /// Encode a normal boxed-value completion.
    #[must_use]
    pub const fn success(value: crate::Value) -> Self {
        Self::success_bits(value.to_abi_bits())
    }

    /// Encode a normal completion from already boxed ABI bits.
    #[must_use]
    pub const fn success_bits(payload_bits: u64) -> Self {
        Self {
            payload_bits,
            status: NativeResultStatus::Success as u64,
        }
    }

    /// Encode a pre-effect side exit.
    ///
    /// Compiled and structured-exception domains pass one typed exit contract.
    /// Probe misses use [`Self::miss`] and its canonical zero payload.
    #[must_use]
    pub const fn side_exit(exit: SideExit) -> Self {
        Self {
            payload_bits: exit.to_bits(),
            status: NativeResultStatus::SideExit as u64,
        }
    }

    /// Encode a pure boxed JavaScript exception.
    ///
    /// Valid only for compiled, structured-exception, and committed domains.
    #[must_use]
    pub const fn throw_value(exception: crate::Value) -> Self {
        Self {
            payload_bits: exception.to_abi_bits(),
            status: NativeResultStatus::Throw as u64,
        }
    }

    /// Report a JavaScript exception already parked by a probe boundary.
    ///
    /// Unlike [`Self::throw_value`], the payload is canonical zero and carries
    /// no moving GC value.
    #[must_use]
    pub const fn throw_pending() -> Self {
        Self {
            payload_bits: 0,
            status: NativeResultStatus::Throw as u64,
        }
    }

    /// Report a probe miss with no committed source effect.
    #[must_use]
    pub const fn miss() -> Self {
        Self {
            payload_bits: 0,
            status: NativeResultStatus::SideExit as u64,
        }
    }

    /// Keep generated fallthrough after a committed structured transition.
    #[must_use]
    pub const fn continue_generated() -> Self {
        Self {
            payload_bits: 0,
            status: NativeResultStatus::Continue as u64,
        }
    }

    /// Report allocation failure from a probe/allocation boundary.
    #[must_use]
    pub const fn out_of_memory() -> Self {
        Self {
            payload_bits: 0,
            status: NativeResultStatus::OutOfMemory as u64,
        }
    }

    /// Encode an engine failure parked in the active runtime context.
    #[doc(hidden)]
    #[must_use]
    pub const fn fatal_internal() -> Self {
        Self {
            payload_bits: crate::Value::UNDEFINED.to_abi_bits(),
            status: NativeResultStatus::Fatal as u64,
        }
    }

    /// Validate this pair against its externally owned result domain.
    ///
    /// The returned status may be used to select the matching payload accessor.
    /// No unknown status word and no status outside the selected state machine
    /// is accepted.
    #[must_use]
    pub const fn validate(self, domain: NativeResultDomain) -> Option<NativeResultStatus> {
        let status = match self.status {
            0 => NativeResultStatus::Success,
            1 => NativeResultStatus::SideExit,
            2 => NativeResultStatus::Throw,
            3 => NativeResultStatus::Continue,
            4 => NativeResultStatus::OutOfMemory,
            6 => NativeResultStatus::Fatal,
            _ => return None,
        };
        let valid = match domain {
            NativeResultDomain::None => false,
            NativeResultDomain::Compiled => match status {
                NativeResultStatus::Success | NativeResultStatus::Throw => true,
                NativeResultStatus::SideExit => SideExit::from_bits(self.payload_bits).is_some(),
                NativeResultStatus::Fatal => {
                    self.payload_bits == crate::Value::UNDEFINED.to_abi_bits()
                }
                NativeResultStatus::Continue | NativeResultStatus::OutOfMemory => false,
            },
            NativeResultDomain::ExceptionTransition => match status {
                NativeResultStatus::Success | NativeResultStatus::Throw => true,
                NativeResultStatus::SideExit => SideExit::from_bits(self.payload_bits).is_some(),
                NativeResultStatus::Continue => self.payload_bits == 0,
                NativeResultStatus::Fatal => {
                    self.payload_bits == crate::Value::UNDEFINED.to_abi_bits()
                }
                NativeResultStatus::OutOfMemory => false,
            },
            NativeResultDomain::Committed => match status {
                NativeResultStatus::Success | NativeResultStatus::Throw => true,
                NativeResultStatus::Fatal => {
                    self.payload_bits == crate::Value::UNDEFINED.to_abi_bits()
                }
                NativeResultStatus::SideExit
                | NativeResultStatus::Continue
                | NativeResultStatus::OutOfMemory => false,
            },
            NativeResultDomain::Probe => match status {
                NativeResultStatus::Success => true,
                NativeResultStatus::SideExit
                | NativeResultStatus::Throw
                | NativeResultStatus::OutOfMemory => self.payload_bits == 0,
                NativeResultStatus::Fatal => {
                    self.payload_bits == crate::Value::UNDEFINED.to_abi_bits()
                }
                NativeResultStatus::Continue => false,
            },
        };
        if valid { Some(status) } else { None }
    }

    /// Raw `x0` payload bits.
    #[must_use]
    pub const fn payload_bits(self) -> u64 {
        self.payload_bits
    }

    /// Decode a validated Success or pure Throw payload as a boxed value.
    #[must_use]
    pub const fn payload_value(self) -> crate::Value {
        crate::Value::from_abi_bits(self.payload_bits)
    }

    /// Decode a validated compiled/exception side-exit payload.
    #[must_use]
    pub const fn side_exit_payload(self) -> Option<SideExit> {
        SideExit::from_bits(self.payload_bits)
    }

    /// Decode a validated side exit's logical PC.
    #[must_use]
    pub const fn logical_pc(self) -> Option<u32> {
        match self.side_exit_payload() {
            Some(exit) => Some(exit.logical_pc()),
            None => None,
        }
    }
}

const _: [(); 16] = [(); std::mem::size_of::<NativeResultPair>()];
const _: [(); 8] = [(); std::mem::align_of::<NativeResultPair>()];
const _: [(); 0] = [(); std::mem::offset_of!(NativeResultPair, payload_bits)];
const _: [(); 8] = [(); std::mem::offset_of!(NativeResultPair, status)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_pair_validates_every_domain_subset() {
        let value = crate::Value::number_i32(42);
        let success = NativeResultPair::success(value);
        for domain in [
            NativeResultDomain::Compiled,
            NativeResultDomain::ExceptionTransition,
            NativeResultDomain::Committed,
            NativeResultDomain::Probe,
        ] {
            assert_eq!(success.validate(domain), Some(NativeResultStatus::Success));
            assert_eq!(success.payload_value(), value);
        }

        let contract = SideExit::new(17, ExitReason::TypeMismatch, ExitAction::Recompile);
        let side_exit = NativeResultPair::side_exit(contract);
        assert_eq!(
            side_exit.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(side_exit.logical_pc(), Some(17));
        assert_eq!(side_exit.side_exit_payload(), Some(contract));
        assert_eq!(
            side_exit.validate(NativeResultDomain::ExceptionTransition),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(side_exit.validate(NativeResultDomain::Committed), None);
        assert_eq!(side_exit.validate(NativeResultDomain::Probe), None);

        let miss = NativeResultPair::miss();
        assert_eq!(
            miss.validate(NativeResultDomain::Probe),
            Some(NativeResultStatus::SideExit)
        );
    }

    #[test]
    fn typed_side_exit_round_trips_every_reason_and_action() {
        let reasons = [
            ExitReason::TypeMismatch,
            ExitReason::Int32Overflow,
            ExitReason::NegativeZero,
            ExitReason::InvalidElementIndex,
            ExitReason::IdentityGuard,
            ExitReason::ShapeGuard,
            ExitReason::BoundsGuard,
            ExitReason::Interrupt,
            ExitReason::AllocationMiss,
            ExitReason::UnsupportedOperation,
            ExitReason::RuntimeTransition,
        ];
        let actions = [
            ExitAction::Resume,
            ExitAction::Recompile,
            ExitAction::Invalidate,
        ];
        for reason in reasons {
            for action in actions {
                let exit = SideExit::new(u32::MAX, reason, action);
                assert_eq!(SideExit::from_bits(exit.to_bits()), Some(exit));
            }
        }

        assert_eq!(SideExit::from_bits(1), None, "reason zero is invalid");
        assert_eq!(
            SideExit::from_bits(1 | (u64::from(ExitReason::TypeMismatch as u8) << 32) | (3 << 40)),
            None,
            "action three is reserved"
        );
        assert_eq!(
            SideExit::from_bits(1 | (0xff << 32)),
            None,
            "unknown reasons are invalid"
        );
    }

    #[test]
    fn pure_and_pending_throw_payloads_are_domain_checked() {
        let exception = crate::Value::number_i32(9);
        let pure = NativeResultPair::throw_value(exception);
        for domain in [
            NativeResultDomain::Compiled,
            NativeResultDomain::ExceptionTransition,
            NativeResultDomain::Committed,
        ] {
            assert_eq!(pure.validate(domain), Some(NativeResultStatus::Throw));
            assert_eq!(pure.payload_value(), exception);
        }
        assert_eq!(pure.validate(NativeResultDomain::Probe), None);

        let pending = NativeResultPair::throw_pending();
        assert_eq!(
            pending.validate(NativeResultDomain::Probe),
            Some(NativeResultStatus::Throw)
        );
        assert_eq!(pending.payload_bits(), 0);
    }

    #[test]
    fn continue_and_out_of_memory_are_domain_exclusive() {
        let continued = NativeResultPair::continue_generated();
        assert_eq!(
            continued.validate(NativeResultDomain::ExceptionTransition),
            Some(NativeResultStatus::Continue)
        );
        for domain in [
            NativeResultDomain::Compiled,
            NativeResultDomain::Committed,
            NativeResultDomain::Probe,
            NativeResultDomain::None,
        ] {
            assert_eq!(continued.validate(domain), None);
        }

        let oom = NativeResultPair::out_of_memory();
        assert_eq!(
            oom.validate(NativeResultDomain::Probe),
            Some(NativeResultStatus::OutOfMemory)
        );
        assert_eq!(oom.validate(NativeResultDomain::Compiled), None);
    }

    #[test]
    fn malformed_status_and_payload_words_are_rejected() {
        let unknown = NativeResultPair {
            payload_bits: 0,
            status: 5,
        };
        let widened_status = NativeResultPair {
            payload_bits: 0,
            status: 1 << 8,
        };
        let malformed_exit = NativeResultPair {
            payload_bits: 1_u64 << 63,
            status: NativeResultStatus::SideExit as u64,
        };
        for domain in [
            NativeResultDomain::Compiled,
            NativeResultDomain::ExceptionTransition,
            NativeResultDomain::Committed,
            NativeResultDomain::Probe,
        ] {
            assert_eq!(unknown.validate(domain), None);
            assert_eq!(widened_status.validate(domain), None);
        }
        assert_eq!(malformed_exit.validate(NativeResultDomain::Compiled), None);
        assert_eq!(malformed_exit.validate(NativeResultDomain::Probe), None);
    }
}
