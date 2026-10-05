//! Strict persistent warm observations converted to existing benchmark metrics.
//!
//! # Contents
//! - Ordered setup, three excluded warmups, five measured invocations and completion.
//! - Exact integer nanoseconds and per-invocation result/predicate validation.
//!
//! # Invariants
//! - Partial, duplicate, foreign and lossy observations never become measurements.
//! - All warmups are validated; the first sample cannot become an oracle.
//! - Process exit/watchdog/provenance checks belong to the capture driver.
//!
//! # See also
//! - `super::emit` is the sole producer of these records.

use super::emit::{CLOCK, PREFIX, SCOPE};
use super::{
    ValidatedWarmRun, WarmHarnessError, WarmHarnessManifest, WarmPhase, WarmRecord, reject,
};

pub(super) fn validate(
    manifest: &WarmHarnessManifest,
    stdout: &[u8],
) -> Result<ValidatedWarmRun, WarmHarnessError> {
    let expected = manifest.expected_result.as_ref().ok_or_else(|| {
        reject("untimed original-result oracle must be frozen before validating/scoring crypto")
    })?;
    if manifest.original_sha256 != manifest.anchor.original_sha256()
        || manifest.clock != CLOCK
        || manifest.scope != SCOPE
        || manifest.host_environment != "classic-script-shell"
        || manifest.sampling.warmup_count < 3
        || manifest.sampling.sample_count < 5
        || manifest.sampling.iterations_per_sample.is_some()
    {
        return Err(reject(
            "manifest differs from current immutable warm contract",
        ));
    }
    let text = std::str::from_utf8(stdout).map_err(|_| reject("warm output is not UTF-8"))?;
    let mut records = Vec::new();
    for line in text.lines() {
        let json = line
            .strip_prefix(PREFIX)
            .ok_or_else(|| reject("unexpected stdout outside the warm record protocol"))?;
        records.push(
            serde_json::from_str::<WarmRecord>(json)
                .map_err(|error| reject(format!("invalid warm record: {error}")))?,
        );
    }
    let count =
        u64::from(manifest.sampling.warmup_count) + u64::from(manifest.sampling.sample_count) + 2;
    if records.len() as u64 != count {
        return Err(reject("incomplete or extra warm records"));
    }
    match &records[0] {
        WarmRecord::Ready {
            anchor,
            original_sha256,
            scope,
            clock,
            warmup_count,
            sample_count,
        } if *anchor == manifest.anchor
            && original_sha256 == &manifest.original_sha256
            && scope == SCOPE
            && clock == CLOCK
            && *warmup_count == manifest.sampling.warmup_count
            && *sample_count == manifest.sampling.sample_count =>
        {
            ()
        }
        _ => return Err(reject("missing or foreign Ready record")),
    }
    match records.last() {
        Some(WarmRecord::Complete {
            anchor,
            warmup_count,
            sample_count,
        }) if *anchor == manifest.anchor
            && *warmup_count == manifest.sampling.warmup_count
            && *sample_count == manifest.sampling.sample_count =>
        {
            ()
        }
        _ => return Err(reject("missing or foreign Complete record")),
    }
    let mut result = ValidatedWarmRun {
        anchor: manifest.anchor,
        warmups: Vec::new(),
        samples: Vec::new(),
        measured_ns: Vec::new(),
    };
    for (position, record) in records
        .into_iter()
        .skip(1)
        .take(count as usize - 2)
        .enumerate()
    {
        let WarmRecord::Invocation(observation) = record else {
            return Err(reject(
                "structural record occurs inside invocation sequence",
            ));
        };
        let warm = position < manifest.sampling.warmup_count as usize;
        let index = if warm {
            position
        } else {
            position - manifest.sampling.warmup_count as usize
        };
        if observation.index as usize != index
            || observation.phase
                != if warm {
                    WarmPhase::Warmup
                } else {
                    WarmPhase::Measured
                }
        {
            return Err(reject(
                "duplicate, reordered or incorrectly phased invocation",
            ));
        }
        if &observation.result != expected {
            return Err(reject("original semantic result mismatch"));
        }
        for (key, value) in &manifest.required_checks {
            if observation.checks.get(key) != Some(value) {
                return Err(reject(format!(
                    "missing or failed useful outer check: {key}"
                )));
            }
        }
        if warm {
            if observation.elapsed_ns_decimal.is_some() {
                return Err(reject("warmups must not contribute timed samples"));
            }
            result.warmups.push(observation);
        } else {
            let decimal = observation
                .elapsed_ns_decimal
                .as_deref()
                .ok_or_else(|| reject("measured invocation has no nanoseconds"))?;
            if decimal.is_empty()
                || !decimal.bytes().all(|byte| byte.is_ascii_digit())
                || (decimal.len() > 1 && decimal.starts_with('0'))
            {
                return Err(reject("nanoseconds must be canonical unsigned decimal"));
            }
            let nanos = decimal
                .parse::<u64>()
                .map_err(|_| reject("nanoseconds exceed u64"))?;
            if nanos == 0 {
                return Err(reject("measured elapsed time must be positive"));
            }
            result.measured_ns.push(nanos);
            result.samples.push(observation);
        }
    }
    Ok(result)
}
