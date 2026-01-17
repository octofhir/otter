//! Otter KV - Key-value store for Otter runtime
//!
//! Provides a simple key-value store API using redb (pure Rust, no FFI).
//!
//! # Usage
//!
//! ```typescript
//! import { kv } from "otter";
//!
//! const store = kv("./data.kv");      // file-based
//! const store = kv(":memory:");       // in-memory
//!
//! store.set("key", { any: "value" });
//! store.get("key");                   // { any: "value" }
//! store.delete("key");                // true
//! store.has("key");                   // false
//! store.keys();                       // []
//! store.clear();
//! store.close();
//! ```

mod extension;
mod store;

pub use extension::kv_extension;
pub use store::{KvError, KvResult, KvStore};

/// JS wrapper code for the KV module
pub const KV_JS: &str = include_str!("kv.js");
