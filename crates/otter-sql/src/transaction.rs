//! Transaction handling for SQL operations

use crate::adapter::IsolationLevel;

/// Transaction state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    Active,
    Committed,
    RolledBack,
}

/// Savepoint counter for generating unique names
static SAVEPOINT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn next_savepoint_name() -> String {
    let id = SAVEPOINT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("sp_{}", id)
}

/// Helper to format isolation level for SQL
pub fn isolation_level_sql(level: IsolationLevel, _is_postgres: bool) -> &'static str {
    match level {
        IsolationLevel::ReadCommitted => "READ COMMITTED",
        IsolationLevel::RepeatableRead => "REPEATABLE READ",
        IsolationLevel::Serializable => "SERIALIZABLE",
    }
}
