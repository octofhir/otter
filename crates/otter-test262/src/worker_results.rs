//! Strict admission of private process-worker result streams.
//!
//! # Contents
//! - [`WorkerLine`] carries a test index and the sole owned result row.
//! - [`read_rows`] verifies a contiguous prefix of an assigned test chunk.
//!
//! # Invariants
//! Malformed JSON, duplicate/out-of-range indices and wrong path identities
//! fail the run before publication. A worker may retire after a valid prefix;
//! it cannot substitute another test or silently omit a hole in that prefix.
//!
//! # See also
//! - `main.rs` owns process isolation and progress; `report` checks coverage.

use std::ops::Range;
use std::path::Path;

use otter_test262::results::TestResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerLine {
    pub(crate) idx: usize,
    pub(crate) result: TestResult,
}

pub(crate) fn read_rows(
    file: &Path,
    expected: &[String],
    assigned: Range<usize>,
    successful: bool,
) -> Result<Vec<WorkerLine>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("worker result read failed: {error}")),
    };
    let mut rows = Vec::new();
    for (line, text) in text.lines().enumerate() {
        let row: WorkerLine = serde_json::from_str(text)
            .map_err(|error| format!("malformed worker row {}: {error}", line + 1))?;
        if !assigned.contains(&row.idx) || row.idx != assigned.start + rows.len() {
            return Err(format!(
                "worker index {} is not the next assigned index {} in {assigned:?}",
                row.idx,
                assigned.start + rows.len()
            ));
        }
        let path = expected
            .get(row.idx)
            .ok_or_else(|| format!("worker index {} exceeds test selection", row.idx))?;
        if row.result.path != *path {
            return Err(format!(
                "worker index {} expected path {path:?}, got {:?}",
                row.idx, row.result.path
            ));
        }
        rows.push(row);
    }
    if successful && rows.is_empty() && !assigned.is_empty() {
        return Err("successful worker produced no test rows".to_owned());
    }
    if !successful && rows.len() == assigned.len() && !assigned.is_empty() {
        return Err("abnormal worker exit after reporting its complete chunk".to_owned());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_test262::results::{Outcome, SkipReason};

    fn line(idx: usize, path: &str, outcome: Outcome) -> String {
        serde_json::to_string(&WorkerLine {
            idx,
            result: TestResult {
                path: path.to_owned(),
                esid: None,
                features: Vec::new(),
                outcome,
                wall_ms: 0,
            },
        })
        .unwrap()
    }

    #[test]
    fn admits_actual_pass_and_skip_prefix_but_rejects_wrong_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.jsonl");
        let expected = vec!["a.js".to_owned(), "b.js".to_owned(), "c.js".to_owned()];
        let rows = [
            line(0, "a.js", Outcome::Pass),
            line(
                1,
                "b.js",
                Outcome::Skipped {
                    reason: SkipReason::MissingFrontmatter,
                },
            ),
        ]
        .join("\n");
        std::fs::write(&file, &rows).unwrap();
        assert_eq!(read_rows(&file, &expected, 0..3, true).unwrap().len(), 2);
        std::fs::write(&file, line(0, "b.js", Outcome::Pass)).unwrap();
        assert!(
            read_rows(&file, &expected, 0..3, true)
                .unwrap_err()
                .contains("expected path")
        );
    }

    #[test]
    fn rejects_duplicates_holes_outside_chunk_and_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.jsonl");
        let expected = vec!["a.js".to_owned(), "b.js".to_owned()];
        let first = line(0, "a.js", Outcome::Pass);
        for text in [
            format!("{first}\n{first}"),
            line(1, "b.js", Outcome::Pass),
            format!("{first}\n{{invalid"),
            line(2, "c.js", Outcome::Pass),
        ] {
            std::fs::write(&file, text).unwrap();
            assert!(read_rows(&file, &expected, 0..2, true).is_err());
        }
        std::fs::write(&file, first).unwrap();
        assert!(read_rows(&file, &expected, 1..2, true).is_err());
    }
    #[test]
    fn rejects_empty_success_and_abnormal_status_after_complete_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.jsonl");
        let expected = vec!["a.js".to_owned(), "b.js".to_owned()];
        assert!(read_rows(&file, &expected, 0..2, true).is_err());
        assert!(read_rows(&file, &expected, 0..2, false).unwrap().is_empty());
        std::fs::write(&file, line(0, "a.js", Outcome::Pass)).unwrap();
        assert_eq!(read_rows(&file, &expected, 0..2, false).unwrap().len(), 1);
        assert_eq!(read_rows(&file, &expected, 0..2, true).unwrap().len(), 1);
        std::fs::write(
            &file,
            [
                line(0, "a.js", Outcome::Pass),
                line(1, "b.js", Outcome::Pass),
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(read_rows(&file, &expected, 0..2, false).is_err());
        assert_eq!(read_rows(&file, &expected, 0..2, true).unwrap().len(), 2);
    }
}
