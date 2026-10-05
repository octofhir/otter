//! Immutable source admission for benchmark bytecode bodies.
//!
//! # Contents
//! - `registry` retains the exact source passed to the compiler.
//!
//! # Invariants
//! Admission and line-index construction happen before linking and outside
//! measured useful invocations. A source refusal uses the existing owned
//! benchmark failure domain; the original kernel is unchanged.
//!
//! # See also
//! - `otter_vm::source_registry` owns immutable text/index lifetime.

use super::{RunFailure, RunFailureKind};
use std::collections::BTreeMap;

pub(super) fn registry(
    source: &str,
    url: &str,
    account: &otter_runtime::ResourceAccount,
) -> Result<otter_vm::source_registry::SourceRegistry, RunFailure> {
    let text = otter_runtime::SharedSource::admit(account, source.to_owned()).map_err(|error| {
        RunFailure {
            kind: RunFailureKind::Compile,
            message: format!("source admission failed: {error}"),
        }
    })?;
    otter_vm::source_registry::SourceRegistry::new(
        BTreeMap::from([(url.to_owned(), text)]),
        account,
    )
    .map_err(|error| RunFailure {
        kind: RunFailureKind::Compile,
        message: format!("source index admission failed: {error}"),
    })
}
