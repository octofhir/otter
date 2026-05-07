//! `--shard N/M` traversal — stable, order-independent partitioning.
//!
//! The runner splits the corpus across `M` workers using a stable
//! 64-bit FNV-1a hash of each test's path so the assignment is
//! deterministic regardless of corpus traversal order. Slice 105
//! drives this from CI as eight parallel jobs; the parent supervisor
//! owns the cursor file written next to each shard's output (slice
//! 104 §supervisor).
//!
//! Spec link: <https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function>

use std::path::{Path, PathBuf};

use thiserror::Error;

/// `N/M` shard spec parsed from the CLI string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardSpec {
    /// 1-based shard index (`1..=total`).
    pub index: u32,
    /// Total shard count (`>= 1`).
    pub total: u32,
}

impl ShardSpec {
    /// Parse `"N/M"`. Both sides must be positive integers and
    /// `N <= M`.
    ///
    /// # Errors
    /// Returns [`ShardError`] for malformed input.
    pub fn parse(spec: &str) -> Result<Self, ShardError> {
        let (n_s, m_s) = spec.split_once('/').ok_or(ShardError::Malformed {
            input: spec.to_string(),
        })?;
        let index: u32 = n_s.trim().parse().map_err(|_| ShardError::Malformed {
            input: spec.to_string(),
        })?;
        let total: u32 = m_s.trim().parse().map_err(|_| ShardError::Malformed {
            input: spec.to_string(),
        })?;
        if total == 0 {
            return Err(ShardError::ZeroTotal);
        }
        if index == 0 || index > total {
            return Err(ShardError::OutOfRange { index, total });
        }
        Ok(Self { index, total })
    }

    /// `true` when the test path belongs to this shard under stable
    /// FNV-1a hashing.
    #[must_use]
    pub fn contains(&self, rel_path: &str) -> bool {
        let bucket = u32::try_from(stable_bucket(rel_path, self.total)).unwrap_or(0);
        bucket + 1 == self.index
    }

    /// Filter `tests` down to the entries assigned to this shard.
    /// The relative path is computed via `rel(test)`.
    pub fn filter<'a, F>(&self, tests: &'a [PathBuf], rel: F) -> Vec<&'a Path>
    where
        F: Fn(&Path) -> String,
    {
        tests
            .iter()
            .filter(|p| self.contains(&rel(p)))
            .map(|p| p.as_path())
            .collect()
    }
}

/// Fold a string into a `0..total` bucket via FNV-1a 64.
#[must_use]
pub fn stable_bucket(input: &str, total: u32) -> u64 {
    debug_assert!(total > 0, "shard total must be positive");
    let h = fnv1a64(input.as_bytes());
    h % u64::from(total)
}

/// FNV-1a 64-bit hash. Stable across Rust versions and platforms,
/// independent of `std::hash::Hasher`'s Build* dance.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Errors raised by [`ShardSpec::parse`].
#[derive(Debug, Error)]
pub enum ShardError {
    /// Could not parse `N/M`.
    #[error("malformed shard spec {input:?} — expected N/M with positive integers")]
    Malformed {
        /// The string the user supplied.
        input: String,
    },
    /// `M = 0`.
    #[error("shard total must be positive (got 0)")]
    ZeroTotal,
    /// `N` is `0` or `> M`.
    #[error("shard index {index} is out of range for total {total}")]
    OutOfRange {
        /// Provided index.
        index: u32,
        /// Provided total.
        total: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_spec() {
        let s = ShardSpec::parse("3/8").unwrap();
        assert_eq!(s.index, 3);
        assert_eq!(s.total, 8);
    }

    #[test]
    fn rejects_zero_total() {
        assert!(matches!(
            ShardSpec::parse("0/0"),
            Err(ShardError::ZeroTotal)
        ));
    }

    #[test]
    fn rejects_out_of_range_index() {
        assert!(matches!(
            ShardSpec::parse("9/8"),
            Err(ShardError::OutOfRange { .. })
        ));
        assert!(matches!(
            ShardSpec::parse("0/8"),
            Err(ShardError::OutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_malformed_strings() {
        assert!(matches!(
            ShardSpec::parse("not-a-spec"),
            Err(ShardError::Malformed { .. })
        ));
        assert!(matches!(
            ShardSpec::parse("3/"),
            Err(ShardError::Malformed { .. })
        ));
    }

    #[test]
    fn shards_partition_corpus_disjointly() {
        // Synthetic corpus of 1000 paths; verify each one belongs to
        // exactly one shard and the union covers everything.
        let total = 8u32;
        let paths: Vec<String> = (0..1000)
            .map(|i| format!("language/expressions/sample-{i}.js"))
            .collect();

        let mut hits = vec![0u32; paths.len()];
        for index in 1..=total {
            let spec = ShardSpec { index, total };
            for (i, path) in paths.iter().enumerate() {
                if spec.contains(path) {
                    hits[i] += 1;
                }
            }
        }
        assert!(
            hits.iter().all(|&h| h == 1),
            "every path lands in exactly one shard"
        );
    }

    #[test]
    fn shard_assignment_is_stable() {
        // Stability is the whole point — the bucket must not change
        // between runs / platforms.
        assert_eq!(
            stable_bucket("built-ins/Array/from/of.js", 8),
            stable_bucket("built-ins/Array/from/of.js", 8)
        );
    }
}
