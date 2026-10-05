//! Shared parallel assignments for Graph edges and canonical recovery homes.
//!
//! # Contents
//! - [`Emitter`] supplies only target value transfers and aligned cycle parking.
//! - [`emit`] schedules one assignment, preserving cycles and source fanout.
//! - Tests compare every result with simultaneous reads of the initial state.
//!
//! # Invariants
//! - A destination has one final source; identical recovery assignments may
//!   repeat when the same SSA value appears in several reconstructed bindings.
//! - A transfer runs only after all remaining readers of its destination.
//! - Residual cycles park one source outside allocator and emitter scratch.
//! - Parking and unparking balance; target slot offsets track that stack delta.
//! - Constants are rematerialized sources and never assignment destinations.
//!
//! # See also
//! - [`super::regalloc`] owns physical locations and edge/recovery assignments.
//! - [`super::arm64`] supplies one target encoding of these scheduling decisions.

use rustc_hash::FxHashMap;

use super::regalloc::{Location, Move};

/// Zero-cost target operations; the scheduler owns no architecture or carrier.
pub(crate) trait Emitter {
    fn move_value(&mut self, from: Location, to: Location);
    fn park(&mut self, from: Location);
    fn unpark(&mut self, to: Location);
}

/// Run the moves with simultaneous-assignment semantics.
pub(crate) fn emit(emitter: &mut impl Emitter, moves: Vec<Move>) {
    // Recovery can name one SSA value several times. Its physical home still
    // has one assignment; retaining duplicates would corrupt cycle read counts.
    let mut destinations = FxHashMap::default();
    let moves: Vec<Move> = moves
        .into_iter()
        .filter(|m| {
            assert!(
                !matches!(m.to, Location::Constant(_)),
                "a constant cannot be a move destination"
            );
            if let Some(&source) = destinations.get(&m.to) {
                assert_eq!(
                    source, m.from,
                    "conflicting sources of a parallel move destination"
                );
                return false;
            }
            destinations.insert(m.to, m.from);
            m.from != m.to
        })
        .collect();
    if moves.is_empty() {
        return;
    }
    let mut readers: FxHashMap<Location, u32> = FxHashMap::default();
    let mut writer: FxHashMap<Location, usize> = FxHashMap::default();
    for (index, m) in moves.iter().enumerate() {
        *readers.entry(m.from).or_default() += 1;
        writer.insert(m.to, index);
    }
    let mut done = vec![false; moves.len()];
    let mut parked: Option<usize> = None;
    let mut ready: Vec<usize> = (0..moves.len())
        .filter(|&index| !readers.contains_key(&moves[index].to))
        .collect();
    let mut remaining = moves.len();
    while remaining != 0 {
        while let Some(index) = ready.pop() {
            let m = moves[index];
            if parked == Some(index) {
                emitter.unpark(m.to);
                parked = None;
            } else {
                emitter.move_value(m.from, m.to);
                let count = readers.get_mut(&m.from).expect("a read source");
                *count -= 1;
                if *count == 0
                    && let Some(&next) = writer.get(&m.from)
                    && !done[next]
                {
                    ready.push(next);
                }
            }
            done[index] = true;
            remaining -= 1;
        }
        if remaining == 0 {
            break;
        }
        // Fanout drains with the acyclic moves: with distinct destinations,
        // an all-destinations-read remainder is a union of simple cycles.
        assert!(
            parked.is_none(),
            "a parked cycle must drain before the next cycle"
        );
        let index = (0..moves.len())
            .find(|&index| !done[index])
            .expect("a pending move");
        let source = moves[index].from;
        emitter.park(source);
        parked = Some(index);
        let count = readers.get_mut(&source).expect("a read source");
        *count -= 1;
        if *count == 0
            && let Some(&next) = writer.get(&source)
            && !done[next]
        {
            ready.push(next);
        }
    }
    assert!(
        parked.is_none(),
        "parallel assignment must release its parked value"
    );
}

#[cfg(test)]
mod tests {
    use super::super::ir::NodeId;
    use super::*;

    struct Replay {
        values: FxHashMap<Location, u64>,
        parked: Vec<u64>,
        max_depth: usize,
    }

    impl Emitter for Replay {
        fn move_value(&mut self, from: Location, to: Location) {
            self.values.insert(to, self.values[&from]);
        }
        fn park(&mut self, from: Location) {
            self.parked.push(self.values[&from]);
            self.max_depth = self.max_depth.max(self.parked.len());
        }
        fn unpark(&mut self, to: Location) {
            self.values
                .insert(to, self.parked.pop().expect("one aligned parked word"));
        }
    }

    fn check(locations: &[Location], moves: Vec<Move>) {
        let initial: FxHashMap<_, _> = locations
            .iter()
            .enumerate()
            .map(|(index, &location)| {
                // Include bit patterns that must retain all 64 bits through FP,
                // tagged and untagged homes, including negative zero and NaN.
                (
                    location,
                    [
                        0x7138_1739_a3c1_99d7_u64,
                        u64::MAX,
                        (-0.0_f64).to_bits(),
                        0x7ff8_0000_0000_0731,
                        f64::INFINITY.to_bits(),
                        f64::NEG_INFINITY.to_bits(),
                        0,
                        0x5a37_0183_8137_0971,
                    ][index],
                )
            })
            .collect();
        let mut expected = initial.clone();
        for m in &moves {
            expected.insert(m.to, initial[&m.from]);
        }
        let mut replay = Replay {
            values: initial,
            parked: vec![],
            max_depth: 0,
        };
        emit(&mut replay, moves);
        assert_eq!(
            replay.values, expected,
            "final values equal independent simultaneous reads"
        );
        assert!(
            replay.parked.is_empty(),
            "all target stack parking must be released"
        );
        assert!(
            replay.max_depth <= 1,
            "every residual simple cycle uses one aligned park"
        );
    }

    #[test]
    fn every_source_assignment_over_six_register_and_home_destinations_is_exact() {
        let locations = [
            Location::Gp(0),
            Location::Gp(1),
            Location::TaggedSlot(0),
            Location::TaggedSlot(1),
            Location::UntaggedSlot(0),
            Location::UntaggedSlot(1),
        ];
        for mut pattern in 0..6_usize.pow(6) {
            let moves = locations
                .iter()
                .map(|&to| {
                    let from = locations[pattern % locations.len()];
                    pattern /= locations.len();
                    Move { from, to }
                })
                .collect();
            check(&locations, moves);
        }
    }

    #[test]
    fn duplicate_recovery_bindings_cycles_fp_homes_and_constant_fanout_are_exact() {
        let locations = [
            Location::Gp(0),
            Location::Gp(1),
            Location::Fp(0),
            Location::Fp(1),
            Location::UntaggedSlot(0),
            Location::TaggedSlot(0),
            Location::TaggedSlot(1),
            Location::Constant(NodeId(90)),
        ];
        let moves = vec![
            Move {
                from: Location::Gp(0),
                to: Location::Gp(1),
            },
            Move {
                from: Location::Gp(0),
                to: Location::Gp(1),
            },
            Move {
                from: Location::Gp(1),
                to: Location::Gp(0),
            },
            Move {
                from: Location::Fp(0),
                to: Location::Fp(1),
            },
            Move {
                from: Location::Fp(1),
                to: Location::UntaggedSlot(0),
            },
            Move {
                from: Location::UntaggedSlot(0),
                to: Location::Fp(0),
            },
            Move {
                from: Location::Constant(NodeId(90)),
                to: Location::TaggedSlot(0),
            },
            Move {
                from: Location::Constant(NodeId(90)),
                to: Location::TaggedSlot(1),
            },
        ];
        check(&locations, moves);
    }

    #[test]
    #[should_panic(expected = "conflicting sources of a parallel move destination")]
    fn conflicting_destinations_do_not_silently_drop_a_writer() {
        check(
            &[Location::Gp(0), Location::Gp(1), Location::Gp(2)],
            vec![
                Move {
                    from: Location::Gp(0),
                    to: Location::Gp(2),
                },
                Move {
                    from: Location::Gp(1),
                    to: Location::Gp(2),
                },
            ],
        );
    }
}
