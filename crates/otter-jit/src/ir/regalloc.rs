//! Backend-independent linear-scan register allocation for SSA values.
//!
//! # Contents
//! - [`LiveInterval`] — one conservative closed interval per SSA value.
//! - [`Allocation`] — deterministic register/spill assignments and phi moves.
//! - [`Allocation::compute`] — Poletto-Sarkar linear scan with spill-furthest-end.
//! - [`Allocation::verify`] — pure structural, interference, and phi-move checks.
//! - [`RegallocError`] — precise construction and verification failures.
//!
//! # Invariants
//! - Program points are dense in CFG block order, with block heads before
//!   instructions, and intervals conservatively cover holes in liveness.
//! - Phi inputs are live through the final point of their normal predecessor.
//! - Overlapping closed intervals never share a register or spill slot.
//! - Phi edge moves preserve parallel-copy semantics; register
//!   `register_count` is a move-only scratch and is never assigned to a value.
//! - Allocation reads immutable SSA, CFG, and liveness data and has no runtime
//!   effect.
//!
//! # See also
//! - [`crate::ir::cfg`]
//! - [`crate::ir::liveness`]
//! - [`crate::ir::ssa`]

use std::collections::{BTreeMap, BTreeSet};

use super::{
    cfg::{BlockId, ControlFlowGraph},
    liveness::Liveness,
    ssa::{SsaFunction, ValueDef, ValueId},
};

/// Storage assigned to one SSA value, or used by an edge move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Location {
    /// Backend register index.
    Register(u8),
    /// Backend-independent spill-slot index.
    Spill(u32),
}

/// Conservative closed live interval for one SSA value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveInterval {
    /// Value described by this interval.
    pub value: ValueId,
    /// Dense definition position.
    pub start: u32,
    /// Furthest dense position at which the value must remain live.
    pub end: u32,
}

/// One sequential copy used to realize a parallel phi copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// Location read before this move writes its destination.
    pub src: Location,
    /// Location overwritten by this move.
    pub dst: Location,
}

/// Sequentialized phi copies on one normal CFG edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeMoves {
    /// Normal predecessor where the copies execute.
    pub predecessor: BlockId,
    /// Successor block whose phis receive the copies.
    pub block: BlockId,
    /// Ordered copies preserving parallel-move semantics.
    pub moves: Vec<Move>,
}

/// Complete deterministic allocation for one SSA function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    /// Value locations indexed by `ValueId.0`.
    pub locations: Box<[Location]>,
    /// One interval per value, stored in `ValueId` order.
    pub intervals: Box<[LiveInterval]>,
    /// Normal-edge moves in `(predecessor, block)` order.
    pub edge_moves: Box<[EdgeMoves]>,
    /// Number of registers available to values.
    pub register_count: u8,
    /// Number of distinct spill slots assigned to values.
    pub spill_slot_count: u32,
}

/// Failure to construct or verify a register allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegallocError {
    /// SSA and CFG block storage lengths differ.
    BlockCountMismatch {
        /// Number of CFG blocks.
        cfg: usize,
        /// Number of SSA blocks.
        ssa: usize,
    },
    /// A corresponding SSA and CFG block has different instruction counts.
    InstructionCountMismatch {
        /// Block with inconsistent instruction storage.
        block: BlockId,
        /// Number of CFG instructions.
        cfg: usize,
        /// Number of SSA instructions.
        ssa: usize,
    },
    /// Dense program-point numbering exceeded `u32`.
    PositionOverflow,
    /// Fresh spill-slot numbering exceeded `u32`.
    SpillSlotOverflow,
    /// A structural SSA reference is outside dense value storage.
    ValueOutOfRange {
        /// Invalid value identity.
        value: ValueId,
        /// Number of valid values.
        value_count: usize,
    },
    /// A value occurs at more than one structural definition point.
    DuplicateDefinition {
        /// Multiply defined value.
        value: ValueId,
    },
    /// A value has no structural definition point.
    MissingDefinition {
        /// Value lacking a definition point.
        value: ValueId,
    },
    /// A block has no linearized point from which to form edge uses.
    EmptyBlock {
        /// Empty block.
        block: BlockId,
    },
    /// A phi input count differs from the normal predecessor count.
    PhiInputCountMismatch {
        /// Phi with invalid inputs.
        phi: ValueId,
        /// Required input count.
        expected: usize,
        /// Stored input count.
        actual: usize,
    },
    /// Stored locations do not cover exactly the SSA values.
    LocationCountMismatch {
        /// Number of SSA values.
        expected: usize,
        /// Number of stored locations.
        actual: usize,
    },
    /// Stored intervals do not cover exactly the SSA values.
    IntervalCountMismatch {
        /// Number of SSA values.
        expected: usize,
        /// Number of stored intervals.
        actual: usize,
    },
    /// An interval is not stored at its value's dense index.
    IntervalValueOrder {
        /// Value required at this interval index.
        expected: ValueId,
        /// Value actually stored there.
        actual: ValueId,
    },
    /// An interval does not begin at its definition position.
    IntervalStartMismatch {
        /// Value with the invalid start.
        value: ValueId,
        /// Required definition position.
        expected: u32,
        /// Stored start position.
        actual: u32,
    },
    /// An interval does not end at its exact furthest required position.
    IntervalEndMismatch {
        /// Value with the invalid end.
        value: ValueId,
        /// Required furthest-live position.
        expected: u32,
        /// Stored end position.
        actual: u32,
    },
    /// A value was assigned the reserved move-only scratch register.
    ScratchAssignedToValue {
        /// Value assigned the scratch.
        value: ValueId,
        /// Reserved scratch register index.
        register: u8,
    },
    /// An assigned register lies beyond the value-register range.
    RegisterOutOfRange {
        /// Value with the invalid assignment.
        value: ValueId,
        /// Invalid register index.
        register: u8,
        /// Number of assignable registers.
        register_count: u8,
    },
    /// Two overlapping intervals share a register.
    RegisterInterference {
        /// First conflicting value.
        first: ValueId,
        /// Second conflicting value.
        second: ValueId,
        /// Shared register.
        register: u8,
    },
    /// A value references a spill slot beyond the stored slot count.
    SpillSlotOutOfRange {
        /// Value with the invalid spill assignment.
        value: ValueId,
        /// Invalid spill slot.
        slot: u32,
        /// Stored spill-slot count.
        spill_slot_count: u32,
    },
    /// Two values share a spill slot; this allocator always uses fresh slots.
    SpillSlotAliasing {
        /// First value assigned the slot.
        first: ValueId,
        /// Second value assigned the slot.
        second: ValueId,
        /// Shared spill slot.
        slot: u32,
    },
    /// Spill slots are not a dense zero-based set.
    SpillSlotCountMismatch {
        /// Dense count implied by value locations.
        expected: u32,
        /// Stored spill-slot count.
        actual: u32,
    },
    /// Stored locations differ from a deterministic faithful linear-scan replay.
    LinearScanLocationMismatch {
        /// Value with the non-canonical location.
        value: ValueId,
        /// Location produced by the deterministic replay.
        expected: Location,
        /// Stored location.
        actual: Location,
    },
    /// Stored edge-move coverage or ordering differs from normal CFG edges.
    EdgeOrderMismatch {
        /// Edge index at which ordering diverged.
        index: usize,
        /// Expected edge, or `None` past the expected edge list.
        expected: Option<(BlockId, BlockId)>,
        /// Stored edge, or `None` past the stored edge list.
        actual: Option<(BlockId, BlockId)>,
    },
    /// Parallel phi copies require incompatible values in one destination.
    ParallelDestinationConflict {
        /// Edge containing the conflicting copies.
        predecessor: BlockId,
        /// Phi block containing the destination.
        block: BlockId,
        /// Conflicting destination.
        destination: Location,
    },
    /// An edge move uses a register or spill outside the allocation's ranges.
    InvalidMoveLocation {
        /// Edge containing the invalid move.
        predecessor: BlockId,
        /// Successor block containing the phis.
        block: BlockId,
        /// Invalid location.
        location: Location,
    },
    /// Sequential phi simulation read a scratch/location before it held a value.
    UninitializedMoveSource {
        /// Edge containing the invalid move.
        predecessor: BlockId,
        /// Successor block containing the phis.
        block: BlockId,
        /// Uninitialized source location.
        source: Location,
    },
    /// Sequential moves do not deliver one phi's predecessor input.
    PhiMoveIncomplete {
        /// Edge containing the invalid moves.
        predecessor: BlockId,
        /// Successor block containing the phi.
        block: BlockId,
        /// Phi whose destination has the wrong value.
        phi: ValueId,
        /// Original input location that must arrive.
        expected: Location,
        /// Original location whose value actually arrived, if any.
        actual: Option<Location>,
    },
    /// Stored edge moves differ from deterministic sequentialization.
    EdgeMovesMismatch {
        /// Edge containing non-canonical moves.
        predecessor: BlockId,
        /// Successor block containing the phis.
        block: BlockId,
        /// Canonical sequential move list.
        expected: Vec<Move>,
        /// Stored move list.
        actual: Vec<Move>,
    },
}

impl std::fmt::Display for RegallocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid SSA register allocation: {self:?}")
    }
}

impl std::error::Error for RegallocError {}

#[derive(Debug, Clone, Copy)]
enum PointKind {
    Head,
    Instruction(usize),
}

#[derive(Debug, Clone, Copy)]
struct ProgramPoint {
    block: BlockId,
    position: u32,
    kind: PointKind,
}

struct Linearization {
    points: Vec<ProgramPoint>,
    definition_positions: Box<[Option<u32>]>,
    block_last_positions: Box<[u32]>,
}

impl Allocation {
    /// Compute intervals, a faithful linear-scan allocation, and phi edge moves.
    pub fn compute(
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
        liveness: &Liveness,
        register_count: u8,
    ) -> Result<Self, RegallocError> {
        let linear = linearize(ssa, cfg)?;
        let intervals = build_intervals(ssa, cfg, liveness, &linear)?;
        let (locations, spill_slot_count) = linear_scan(&intervals, register_count)?;
        let edge_moves = build_edge_moves(ssa, cfg, &locations, register_count)?;
        Ok(Self {
            locations: locations.into_boxed_slice(),
            intervals: intervals.into_boxed_slice(),
            edge_moves: edge_moves.into_boxed_slice(),
            register_count,
            spill_slot_count,
        })
    }

    /// Return the location assigned to `value`.
    #[must_use]
    pub fn location(&self, value: ValueId) -> Location {
        self.locations[value.0 as usize]
    }

    /// Independently verify interval, allocation, and phi-copy invariants.
    pub fn verify(
        &self,
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
        liveness: &Liveness,
    ) -> Result<(), RegallocError> {
        let value_count = ssa.values.len();
        if self.locations.len() != value_count {
            return Err(RegallocError::LocationCountMismatch {
                expected: value_count,
                actual: self.locations.len(),
            });
        }
        if self.intervals.len() != value_count {
            return Err(RegallocError::IntervalCountMismatch {
                expected: value_count,
                actual: self.intervals.len(),
            });
        }

        let linear = linearize(ssa, cfg)?;
        verify_intervals(&self.intervals, ssa, cfg, liveness, &linear)?;
        self.verify_locations()?;
        self.verify_interference()?;
        self.verify_spills()?;

        let (expected_locations, expected_spills) =
            linear_scan(&self.intervals, self.register_count)?;
        for (index, (&expected, &actual)) in expected_locations
            .iter()
            .zip(self.locations.iter())
            .enumerate()
        {
            if expected != actual {
                return Err(RegallocError::LinearScanLocationMismatch {
                    value: ValueId(index as u32),
                    expected,
                    actual,
                });
            }
        }
        if expected_spills != self.spill_slot_count {
            return Err(RegallocError::SpillSlotCountMismatch {
                expected: expected_spills,
                actual: self.spill_slot_count,
            });
        }

        self.verify_edge_moves(ssa, cfg)
    }

    fn verify_locations(&self) -> Result<(), RegallocError> {
        for (index, &location) in self.locations.iter().enumerate() {
            if let Location::Register(register) = location {
                let value = ValueId(index as u32);
                if register == self.register_count {
                    return Err(RegallocError::ScratchAssignedToValue { value, register });
                }
                if register > self.register_count {
                    return Err(RegallocError::RegisterOutOfRange {
                        value,
                        register,
                        register_count: self.register_count,
                    });
                }
            }
        }
        Ok(())
    }

    fn verify_interference(&self) -> Result<(), RegallocError> {
        for first_index in 0..self.intervals.len() {
            for second_index in (first_index + 1)..self.intervals.len() {
                let first = self.intervals[first_index];
                let second = self.intervals[second_index];
                if intervals_overlap(first, second)
                    && let (Location::Register(first_register), Location::Register(second_register)) =
                        (self.locations[first_index], self.locations[second_index])
                    && first_register == second_register
                {
                    return Err(RegallocError::RegisterInterference {
                        first: first.value,
                        second: second.value,
                        register: first_register,
                    });
                }
            }
        }
        Ok(())
    }

    fn verify_spills(&self) -> Result<(), RegallocError> {
        let mut owners = BTreeMap::new();
        for (index, &location) in self.locations.iter().enumerate() {
            let Location::Spill(slot) = location else {
                continue;
            };
            let value = ValueId(index as u32);
            if slot >= self.spill_slot_count {
                return Err(RegallocError::SpillSlotOutOfRange {
                    value,
                    slot,
                    spill_slot_count: self.spill_slot_count,
                });
            }
            if let Some(first) = owners.insert(slot, value) {
                return Err(RegallocError::SpillSlotAliasing {
                    first,
                    second: value,
                    slot,
                });
            }
        }
        let expected = u32::try_from(owners.len()).map_err(|_| RegallocError::SpillSlotOverflow)?;
        if expected != self.spill_slot_count || owners.keys().copied().ne(0..self.spill_slot_count)
        {
            return Err(RegallocError::SpillSlotCountMismatch {
                expected,
                actual: self.spill_slot_count,
            });
        }
        Ok(())
    }

    fn verify_edge_moves(
        &self,
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
    ) -> Result<(), RegallocError> {
        let expected_edges = normal_edges(cfg);
        let edge_count = expected_edges.len().max(self.edge_moves.len());
        for index in 0..edge_count {
            let expected = expected_edges.get(index).copied();
            let actual = self
                .edge_moves
                .get(index)
                .map(|edge| (edge.predecessor, edge.block));
            if expected != actual {
                return Err(RegallocError::EdgeOrderMismatch {
                    index,
                    expected,
                    actual,
                });
            }
        }

        for edge in &self.edge_moves {
            let parallel =
                parallel_phi_moves(ssa, cfg, &self.locations, edge.predecessor, edge.block)?;
            for movement in &edge.moves {
                for location in [movement.src, movement.dst] {
                    if !self.valid_move_location(location) {
                        return Err(RegallocError::InvalidMoveLocation {
                            predecessor: edge.predecessor,
                            block: edge.block,
                            location,
                        });
                    }
                }
            }

            let mut contents = BTreeMap::new();
            for &location in &self.locations {
                contents.insert(location, location);
            }
            for movement in &edge.moves {
                let Some(&value) = contents.get(&movement.src) else {
                    return Err(RegallocError::UninitializedMoveSource {
                        predecessor: edge.predecessor,
                        block: edge.block,
                        source: movement.src,
                    });
                };
                contents.insert(movement.dst, value);
            }
            for (phi, movement) in
                phi_move_requirements(ssa, cfg, &self.locations, edge.predecessor, edge.block)?
            {
                let actual = contents.get(&movement.dst).copied();
                if actual != Some(movement.src) {
                    return Err(RegallocError::PhiMoveIncomplete {
                        predecessor: edge.predecessor,
                        block: edge.block,
                        phi,
                        expected: movement.src,
                        actual,
                    });
                }
            }

            let expected = sequentialize_parallel_moves(
                parallel,
                Location::Register(self.register_count),
                edge.predecessor,
                edge.block,
            )?;
            if expected != edge.moves {
                return Err(RegallocError::EdgeMovesMismatch {
                    predecessor: edge.predecessor,
                    block: edge.block,
                    expected,
                    actual: edge.moves.clone(),
                });
            }
        }
        Ok(())
    }

    fn valid_move_location(&self, location: Location) -> bool {
        match location {
            Location::Register(register) => register <= self.register_count,
            Location::Spill(slot) => slot < self.spill_slot_count,
        }
    }
}

fn linearize(ssa: &SsaFunction, cfg: &ControlFlowGraph) -> Result<Linearization, RegallocError> {
    if ssa.blocks.len() != cfg.blocks.len() {
        return Err(RegallocError::BlockCountMismatch {
            cfg: cfg.blocks.len(),
            ssa: ssa.blocks.len(),
        });
    }
    let mut points = Vec::new();
    let mut definition_positions = vec![None; ssa.values.len()];
    let mut block_last_positions = Vec::with_capacity(cfg.blocks.len());

    for (block_index, cfg_block) in cfg.blocks.iter().enumerate() {
        let block = BlockId(block_index as u32);
        let ssa_block = &ssa.blocks[block_index];
        if cfg_block.instr_pcs.len() != ssa_block.instrs.len() {
            return Err(RegallocError::InstructionCountMismatch {
                block,
                cfg: cfg_block.instr_pcs.len(),
                ssa: ssa_block.instrs.len(),
            });
        }
        for &head in &ssa_block.phis {
            push_point(
                &mut points,
                &mut definition_positions,
                block,
                PointKind::Head,
                Some(head),
                ssa.values.len(),
            )?;
        }
        for instruction_index in 0..ssa_block.instrs.len() {
            let result = ssa_block.instrs[instruction_index].result;
            push_point(
                &mut points,
                &mut definition_positions,
                block,
                PointKind::Instruction(instruction_index),
                result,
                ssa.values.len(),
            )?;
        }
        let Some(last) = points.last().filter(|point| point.block == block) else {
            return Err(RegallocError::EmptyBlock { block });
        };
        block_last_positions.push(last.position);
    }

    for (index, position) in definition_positions.iter().enumerate() {
        if position.is_none() {
            return Err(RegallocError::MissingDefinition {
                value: ValueId(index as u32),
            });
        }
    }
    Ok(Linearization {
        points,
        definition_positions: definition_positions.into_boxed_slice(),
        block_last_positions: block_last_positions.into_boxed_slice(),
    })
}

fn push_point(
    points: &mut Vec<ProgramPoint>,
    definitions: &mut [Option<u32>],
    block: BlockId,
    kind: PointKind,
    defined: Option<ValueId>,
    value_count: usize,
) -> Result<(), RegallocError> {
    let position = u32::try_from(points.len()).map_err(|_| RegallocError::PositionOverflow)?;
    if let Some(value) = defined {
        let index = value_index(value, value_count)?;
        if definitions[index].replace(position).is_some() {
            return Err(RegallocError::DuplicateDefinition { value });
        }
    }
    points.push(ProgramPoint {
        block,
        position,
        kind,
    });
    Ok(())
}

fn build_intervals(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    liveness: &Liveness,
    linear: &Linearization,
) -> Result<Vec<LiveInterval>, RegallocError> {
    let mut intervals = Vec::with_capacity(ssa.values.len());
    for (index, &position) in linear.definition_positions.iter().enumerate() {
        let start = position.ok_or(RegallocError::MissingDefinition {
            value: ValueId(index as u32),
        })?;
        intervals.push(LiveInterval {
            value: ValueId(index as u32),
            start,
            end: start,
        });
    }

    for point in &linear.points {
        let PointKind::Instruction(instruction_index) = point.kind else {
            continue;
        };
        let instruction = &ssa.blocks[point.block.0 as usize].instrs[instruction_index];
        for &input in &instruction.inputs {
            extend_interval(&mut intervals, input, point.position, ssa.values.len())?;
        }
    }

    extend_phi_and_live_out_intervals(ssa, cfg, liveness, linear, &mut intervals)?;
    Ok(intervals)
}

fn extend_phi_and_live_out_intervals(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    liveness: &Liveness,
    linear: &Linearization,
    intervals: &mut [LiveInterval],
) -> Result<(), RegallocError> {
    let value_count = ssa.values.len();
    for (block_index, block) in ssa.blocks.iter().enumerate() {
        let block_id = BlockId(block_index as u32);
        let predecessors = normal_predecessors(cfg, block_id);
        for &head in &block.phis {
            let head_index = value_index(head, value_count)?;
            if let ValueDef::Phi { inputs, .. } = &ssa.values[head_index].def {
                if inputs.len() != predecessors.len() {
                    return Err(RegallocError::PhiInputCountMismatch {
                        phi: head,
                        expected: predecessors.len(),
                        actual: inputs.len(),
                    });
                }
                for (&input, &predecessor) in inputs.iter().zip(&predecessors) {
                    extend_interval(
                        intervals,
                        input,
                        linear.block_last_positions[predecessor.0 as usize],
                        value_count,
                    )?;
                }
            }
        }
        let last = linear.block_last_positions[block_index];
        for &value in liveness.live_out(block_id) {
            extend_interval(intervals, value, last, value_count)?;
        }
    }
    Ok(())
}

fn verify_intervals(
    intervals: &[LiveInterval],
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    liveness: &Liveness,
    linear: &Linearization,
) -> Result<(), RegallocError> {
    let value_count = ssa.values.len();
    let mut required_ends = Vec::with_capacity(value_count);
    for (index, interval) in intervals.iter().enumerate() {
        let value = ValueId(index as u32);
        if interval.value != value {
            return Err(RegallocError::IntervalValueOrder {
                expected: value,
                actual: interval.value,
            });
        }
        let start =
            linear.definition_positions[index].ok_or(RegallocError::MissingDefinition { value })?;
        if interval.start != start {
            return Err(RegallocError::IntervalStartMismatch {
                value,
                expected: start,
                actual: interval.start,
            });
        }
        required_ends.push(start);
    }

    for point in &linear.points {
        let PointKind::Instruction(instruction_index) = point.kind else {
            continue;
        };
        let instruction = &ssa.blocks[point.block.0 as usize].instrs[instruction_index];
        for &input in &instruction.inputs {
            let index = value_index(input, value_count)?;
            required_ends[index] = required_ends[index].max(point.position);
        }
    }
    for (block_index, block) in ssa.blocks.iter().enumerate() {
        let block_id = BlockId(block_index as u32);
        let predecessors = normal_predecessors(cfg, block_id);
        for &head in &block.phis {
            let head_index = value_index(head, value_count)?;
            if let ValueDef::Phi { inputs, .. } = &ssa.values[head_index].def {
                if inputs.len() != predecessors.len() {
                    return Err(RegallocError::PhiInputCountMismatch {
                        phi: head,
                        expected: predecessors.len(),
                        actual: inputs.len(),
                    });
                }
                for (&input, &predecessor) in inputs.iter().zip(&predecessors) {
                    let index = value_index(input, value_count)?;
                    required_ends[index] = required_ends[index]
                        .max(linear.block_last_positions[predecessor.0 as usize]);
                }
            }
        }
        for &value in liveness.live_out(block_id) {
            let index = value_index(value, value_count)?;
            required_ends[index] =
                required_ends[index].max(linear.block_last_positions[block_index]);
        }
    }
    for (index, &expected) in required_ends.iter().enumerate() {
        if intervals[index].end != expected {
            return Err(RegallocError::IntervalEndMismatch {
                value: ValueId(index as u32),
                expected,
                actual: intervals[index].end,
            });
        }
    }
    Ok(())
}

fn extend_interval(
    intervals: &mut [LiveInterval],
    value: ValueId,
    position: u32,
    value_count: usize,
) -> Result<(), RegallocError> {
    let index = value_index(value, value_count)?;
    intervals[index].end = intervals[index].end.max(position);
    Ok(())
}

fn value_index(value: ValueId, value_count: usize) -> Result<usize, RegallocError> {
    let index = value.0 as usize;
    if index >= value_count {
        return Err(RegallocError::ValueOutOfRange { value, value_count });
    }
    Ok(index)
}

fn linear_scan(
    intervals: &[LiveInterval],
    register_count: u8,
) -> Result<(Vec<Location>, u32), RegallocError> {
    let mut order = intervals.to_vec();
    order.sort_by_key(|interval| (interval.start, interval.value));
    let mut locations = vec![Location::Spill(u32::MAX); intervals.len()];
    let mut active: Vec<LiveInterval> = Vec::new();
    let mut free: BTreeSet<u8> = (0..register_count).collect();
    let mut next_spill = 0_u32;

    for interval in order {
        let mut retained = Vec::with_capacity(active.len());
        for old in active.drain(..) {
            if old.end < interval.start {
                let Location::Register(register) = locations[old.value.0 as usize] else {
                    unreachable!("active intervals always occupy registers");
                };
                free.insert(register);
            } else {
                retained.push(old);
            }
        }
        active = retained;

        if register_count == 0 {
            locations[interval.value.0 as usize] = Location::Spill(next_spill);
            next_spill = next_spill
                .checked_add(1)
                .ok_or(RegallocError::SpillSlotOverflow)?;
            continue;
        }

        if active.len() == usize::from(register_count) {
            let spill_index = active.len() - 1;
            let spill = active[spill_index];
            if spill.end > interval.end {
                let register = match locations[spill.value.0 as usize] {
                    Location::Register(register) => register,
                    Location::Spill(_) => unreachable!("active intervals always occupy registers"),
                };
                locations[spill.value.0 as usize] = Location::Spill(next_spill);
                next_spill = next_spill
                    .checked_add(1)
                    .ok_or(RegallocError::SpillSlotOverflow)?;
                active.pop();
                locations[interval.value.0 as usize] = Location::Register(register);
                insert_active(&mut active, interval);
            } else {
                locations[interval.value.0 as usize] = Location::Spill(next_spill);
                next_spill = next_spill
                    .checked_add(1)
                    .ok_or(RegallocError::SpillSlotOverflow)?;
            }
        } else {
            let register = free
                .pop_first()
                .expect("a non-full active set has a free register");
            locations[interval.value.0 as usize] = Location::Register(register);
            insert_active(&mut active, interval);
        }
    }
    Ok((locations, next_spill))
}

fn insert_active(active: &mut Vec<LiveInterval>, interval: LiveInterval) {
    let key = (interval.end, interval.value);
    let index = active
        .binary_search_by_key(&key, |candidate| (candidate.end, candidate.value))
        .unwrap_or_else(|index| index);
    active.insert(index, interval);
}

fn intervals_overlap(first: LiveInterval, second: LiveInterval) -> bool {
    first.start <= second.end && second.start <= first.end
}

fn build_edge_moves(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    locations: &[Location],
    register_count: u8,
) -> Result<Vec<EdgeMoves>, RegallocError> {
    normal_edges(cfg)
        .into_iter()
        .map(|(predecessor, block)| {
            let parallel = parallel_phi_moves(ssa, cfg, locations, predecessor, block)?;
            let moves = sequentialize_parallel_moves(
                parallel,
                Location::Register(register_count),
                predecessor,
                block,
            )?;
            Ok(EdgeMoves {
                predecessor,
                block,
                moves,
            })
        })
        .collect()
}

fn normal_edges(cfg: &ControlFlowGraph) -> Vec<(BlockId, BlockId)> {
    let mut edges = Vec::new();
    for predecessor in &cfg.blocks {
        for &block in &predecessor.normal_succs {
            edges.push((predecessor.id, block));
        }
    }
    edges.sort_unstable();
    edges
}

fn parallel_phi_moves(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    locations: &[Location],
    predecessor: BlockId,
    block: BlockId,
) -> Result<Vec<Move>, RegallocError> {
    Ok(
        phi_move_requirements(ssa, cfg, locations, predecessor, block)?
            .into_iter()
            .map(|(_, movement)| movement)
            .filter(|movement| movement.src != movement.dst)
            .collect(),
    )
}

fn phi_move_requirements(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    locations: &[Location],
    predecessor: BlockId,
    block: BlockId,
) -> Result<Vec<(ValueId, Move)>, RegallocError> {
    let predecessors = normal_predecessors(cfg, block);
    let predecessor_index = predecessors
        .iter()
        .position(|&candidate| candidate == predecessor)
        .expect("normal edge source is a normal predecessor");
    let mut requirements = Vec::new();
    for &phi in &ssa.blocks[block.0 as usize].phis {
        let phi_index = value_index(phi, ssa.values.len())?;
        let ValueDef::Phi { inputs, .. } = &ssa.values[phi_index].def else {
            continue;
        };
        if inputs.len() != predecessors.len() {
            return Err(RegallocError::PhiInputCountMismatch {
                phi,
                expected: predecessors.len(),
                actual: inputs.len(),
            });
        }
        let input = inputs[predecessor_index];
        let input_index = value_index(input, ssa.values.len())?;
        requirements.push((
            phi,
            Move {
                src: locations[input_index],
                dst: locations[phi_index],
            },
        ));
    }
    Ok(requirements)
}

fn sequentialize_parallel_moves(
    parallel: Vec<Move>,
    scratch: Location,
    predecessor: BlockId,
    block: BlockId,
) -> Result<Vec<Move>, RegallocError> {
    let mut destinations = BTreeMap::new();
    let mut pending = Vec::new();
    for movement in parallel {
        if movement.src == movement.dst {
            continue;
        }
        if let Some(previous) = destinations.insert(movement.dst, movement.src) {
            if previous != movement.src {
                return Err(RegallocError::ParallelDestinationConflict {
                    predecessor,
                    block,
                    destination: movement.dst,
                });
            }
            continue;
        }
        pending.push(movement);
    }

    let mut result = Vec::new();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|candidate| !pending.iter().any(|movement| movement.src == candidate.dst));
        if let Some(index) = ready {
            result.push(pending.remove(index));
            continue;
        }

        let saved = pending[0].dst;
        result.push(Move {
            src: saved,
            dst: scratch,
        });
        for movement in &mut pending {
            if movement.src == saved {
                movement.src = scratch;
            }
        }
    }
    Ok(result)
}

fn normal_predecessors(cfg: &ControlFlowGraph, block: BlockId) -> Vec<BlockId> {
    cfg.blocks[block.0 as usize]
        .preds
        .iter()
        .copied()
        .filter(|predecessor| {
            cfg.blocks[predecessor.0 as usize]
                .normal_succs
                .contains(&block)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::*;
    use crate::ir::dom::DominatorTree;

    fn snapshot(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, param_count, register_count, instructions)
    }

    fn analyses(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> (ControlFlowGraph, SsaFunction, Liveness) {
        let snapshot = snapshot(param_count, register_count, instructions);
        let cfg = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        cfg.verify().expect("CFG verifies");
        let ssa = SsaFunction::build(&snapshot, &cfg).expect("SSA builds");
        let dom = DominatorTree::compute(&cfg);
        ssa.verify(&cfg, &dom).expect("SSA verifies");
        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &dom)
            .expect("liveness verifies");
        (cfg, ssa, liveness)
    }

    fn block_at(cfg: &ControlFlowGraph, pc: u32) -> BlockId {
        cfg.blocks
            .iter()
            .find(|block| block.start_pc == pc)
            .expect("PC starts a block")
            .id
    }

    fn op_value_at(ssa: &SsaFunction, pc: u32) -> ValueId {
        ssa.values
            .iter()
            .find_map(|value| match value.def {
                ValueDef::Op { pc: owner, .. } if owner == pc => Some(value.id),
                _ => None,
            })
            .expect("instruction has an SSA result")
    }

    fn phi_for(ssa: &SsaFunction, block: BlockId, register: u16) -> ValueId {
        ssa.blocks[block.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(
                    ssa.values[value.0 as usize].def,
                    ValueDef::Phi {
                        register: owner,
                        ..
                    } if owner == register
                )
            })
            .expect("block has requested phi")
    }

    fn edge_moves(allocation: &Allocation, predecessor: BlockId, block: BlockId) -> &EdgeMoves {
        allocation
            .edge_moves
            .iter()
            .find(|edge| edge.predecessor == predecessor && edge.block == block)
            .expect("normal edge has a move list")
    }

    #[test]
    fn straight_line_reuses_register_after_a_value_dies() {
        let (cfg, ssa, liveness) = analyses(
            0,
            2,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(10)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(20)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, 4).expect("allocate");
        allocation.verify(&ssa, &cfg, &liveness).expect("verify");
        assert_eq!(allocation.spill_slot_count, 0);

        let dead = op_value_at(&ssa, 0);
        let later = op_value_at(&ssa, 1);
        assert_eq!(allocation.location(dead), allocation.location(later));
        assert!(
            allocation.intervals[dead.0 as usize].end
                < allocation.intervals[later.0 as usize].start
        );
        assert_eq!(
            allocation,
            Allocation::compute(&ssa, &cfg, &liveness, 4).expect("deterministic replay")
        );
    }

    #[test]
    fn diamond_phi_has_complete_moves_from_both_arms() {
        let (cfg, ssa, liveness) = analyses(
            1,
            3,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(10)],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(20)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, 4).expect("allocate");
        allocation.verify(&ssa, &cfg, &liveness).expect("verify");

        let left = block_at(&cfg, 1);
        let right = block_at(&cfg, 3);
        let join = block_at(&cfg, 4);
        let phi = phi_for(&ssa, join, 1);
        assert!(phi.0 < ssa.values.len() as u32);
        for predecessor in [left, right] {
            let requirements =
                phi_move_requirements(&ssa, &cfg, &allocation.locations, predecessor, join)
                    .expect("phi requirements");
            assert_eq!(requirements.len(), 1);
            let required = requirements[0].1;
            let edge = edge_moves(&allocation, predecessor, join);
            if required.src == required.dst {
                assert!(edge.moves.is_empty(), "identity moves are skipped");
            } else {
                assert_eq!(edge.moves, vec![required]);
            }
        }
    }

    #[test]
    fn loop_phi_interval_spans_backedge_and_has_a_latch_move() {
        let (cfg, ssa, liveness) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(1)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, 4).expect("allocate");
        allocation.verify(&ssa, &cfg, &liveness).expect("verify");

        let header = block_at(&cfg, 2);
        let latch = block_at(&cfg, 3);
        let phi = phi_for(&ssa, header, 1);
        let carried = op_value_at(&ssa, 3);
        assert!(
            allocation.intervals[carried.0 as usize].end
                >= allocation.intervals[phi.0 as usize].start
        );
        let requirements = phi_move_requirements(&ssa, &cfg, &allocation.locations, latch, header)
            .expect("backedge phi requirement");
        assert_eq!(requirements.len(), 1);
        let edge = edge_moves(&allocation, latch, header);
        if requirements[0].1.src == requirements[0].1.dst {
            assert!(edge.moves.is_empty());
        } else {
            assert_eq!(edge.moves, vec![requirements[0].1]);
        }
    }

    #[test]
    fn forced_spill_with_two_registers_remains_interference_free() {
        let (cfg, ssa, liveness) = analyses(
            3,
            4,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, 2).expect("allocate");
        allocation.verify(&ssa, &cfg, &liveness).expect("verify");
        assert!(allocation.spill_slot_count > 0);
        assert!(
            allocation
                .locations
                .iter()
                .any(|location| matches!(location, Location::Spill(_)))
        );

        let synthetic = [
            LiveInterval {
                value: ValueId(0),
                start: 0,
                end: 10,
            },
            LiveInterval {
                value: ValueId(1),
                start: 1,
                end: 4,
            },
            LiveInterval {
                value: ValueId(2),
                start: 2,
                end: 3,
            },
        ];
        let (locations, spill_count) = linear_scan(&synthetic, 2).expect("linear scan");
        assert_eq!(locations[0], Location::Spill(0));
        assert_eq!(locations[2], Location::Register(0));
        assert_eq!(spill_count, 1, "the furthest active interval is spilled");
    }

    #[test]
    fn parallel_phi_swap_breaks_cycle_with_scratch() {
        let (cfg, ssa, liveness) = analyses(
            1,
            3,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(0)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(10)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(11)],
                ),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(20)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(21)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, 2).expect("allocate");
        allocation.verify(&ssa, &cfg, &liveness).expect("verify");

        let left = block_at(&cfg, 1);
        let join = block_at(&cfg, 6);
        let phi_one = phi_for(&ssa, join, 1);
        let phi_two = phi_for(&ssa, join, 2);
        assert_ne!(allocation.location(phi_one), allocation.location(phi_two));
        let edge = edge_moves(&allocation, left, join);
        assert_eq!(edge.moves.len(), 3, "two-copy cycle needs save plus copies");
        assert!(edge.moves.iter().any(|movement| {
            movement.src == Location::Register(allocation.register_count)
                || movement.dst == Location::Register(allocation.register_count)
        }));
        assert!(
            allocation
                .locations
                .iter()
                .all(|&location| { location != Location::Register(allocation.register_count) })
        );
    }

    #[test]
    fn verifier_rejects_overlapping_values_in_one_register() {
        let (cfg, ssa, liveness) = analyses(
            3,
            4,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        let mut allocation = Allocation::compute(&ssa, &cfg, &liveness, 8).expect("allocate");
        let mut pair = None;
        for first in 0..allocation.intervals.len() {
            for second in (first + 1)..allocation.intervals.len() {
                if intervals_overlap(allocation.intervals[first], allocation.intervals[second])
                    && matches!(allocation.locations[first], Location::Register(_))
                    && matches!(allocation.locations[second], Location::Register(_))
                {
                    pair = Some((first, second));
                    break;
                }
            }
            if pair.is_some() {
                break;
            }
        }
        let (first, second) = pair.expect("test has overlapping register values");
        allocation.locations[second] = allocation.locations[first];

        assert!(matches!(
            allocation.verify(&ssa, &cfg, &liveness),
            Err(RegallocError::RegisterInterference {
                first: actual_first,
                second: actual_second,
                ..
            }) if actual_first == ValueId(first as u32) && actual_second == ValueId(second as u32)
        ));
    }
}
