//! # Holt - The Otter's Resource Storage
//!
//! This module provides the `Holt` type - a thread-safe storage for native resources
//! that need to be accessed from JavaScript. Just like an otter's holt (burrow) where
//! it stores its treasures, this is where we keep file handles, sockets, streams, and
//! other native resources.
//!
//! ## Otter Terminology Glossary
//!
//! Otter uses nature-themed naming inspired by otter behavior. This makes the codebase
//! memorable and creates a consistent vocabulary for contributors.
//!
//! | Term | Description | Usage |
//! |------|-------------|-------|
//! | **Holt** | An otter's burrow or den - our thread-safe resource storage | `Holt::new()`, `holt()` |
//! | **Paw** | The ID used to "grip" a resource - like an otter holding food | `Paw` type (u32) |
//! | **hold** | Store a resource in the holt and get a paw grip | `holt.hold(resource)` |
//! | **catch** | Retrieve a resource by its paw ID | `holt.catch::<T>(paw)` |
//! | **release** | Remove a resource from the holt | `holt.release(paw)` |
//! | **slipped_away** | Resource not found (it "slipped" from our grasp) | `HoltError::SlippedAway` |
//! | **wrong_catch** | Type mismatch when catching | `HoltError::WrongCatch` |
//! | **dive** | A native function callable from JS (otter dives for fish) | `#[dive]` attribute |
//! | **swift** | Fast synchronous dive | `#[dive(swift)]` |
//! | **deep** | Async dive that returns a Promise | `#[dive(deep)]` |
//! | **den** | A module/extension (otter's den with treasures) | `den!` macro |
//!
//! ## Why This Naming?
//!
//! 1. **Memorable**: Nature terms are easier to remember than generic CS terms
//! 2. **Consistent**: All terms relate to otters, creating a unified vocabulary
//! 3. **Distinct**: Our API is clearly different from other runtimes, avoiding confusion
//! 4. **Fun**: Code should be enjoyable to write and read
//!
//! ## Example
//!
//! ```ignore
//! use otter_runtime::holt::{Holt, Paw, holt};
//!
//! // Store a TCP socket
//! let socket = TcpStream::connect("127.0.0.1:8080").await?;
//! let paw: Paw = holt().hold(socket);
//!
//! // Later, retrieve it
//! let socket = holt().catch::<TcpStream>(paw)?;
//!
//! // When done, release it
//! holt().release(paw);
//! ```

use dashmap::DashMap;
use std::any::Any;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// A paw grip on a native resource - the ID used to reference it from JavaScript.
///
/// Like an otter's paw holding onto a fish or shellfish, this ID lets us maintain
/// a grip on native resources across the JS/Rust boundary.
///
/// Paws are unique within a single [`Holt`] instance and are never reused.
pub type Paw = u32;

/// Errors that can occur when working with the Holt.
#[derive(Debug, Error, Clone)]
pub enum HoltError {
    /// The resource slipped away - it wasn't found in the holt.
    /// This can happen if the resource was already released or never existed.
    #[error("Resource slipped away (paw {0} not found)")]
    SlippedAway(Paw),

    /// Wrong catch - the resource exists but is a different type than expected.
    /// Like an otter expecting a fish but catching a crab.
    #[error("Wrong catch - type mismatch for paw {0}")]
    WrongCatch(Paw),
}

/// Result type for Holt operations.
pub type HoltResult<T> = Result<T, HoltError>;

/// An otter's holt - thread-safe storage for native resources.
///
/// The Holt stores arbitrary native resources (file handles, sockets, streams, etc.)
/// and provides integer IDs (Paws) to reference them from JavaScript. This is essential
/// for the JS/Rust bridge since JavaScript can't directly hold Rust objects.
///
/// ## Thread Safety
///
/// Holt is fully thread-safe and can be shared across multiple threads using `Arc`.
/// All operations are atomic and lock-free where possible.
///
/// ## Memory Management
///
/// Resources stored in the Holt are reference-counted (`Arc`). When you `hold` a resource,
/// it's wrapped in an `Arc`. When you `catch` it, you get a clone of that `Arc`. The
/// resource is only dropped when both the Holt releases it AND all caught references
/// are dropped.
pub struct Holt {
    /// The stash where resources are stored, indexed by Paw.
    stash: DashMap<Paw, Arc<dyn Any + Send + Sync>>,

    /// Counter for generating unique Paw IDs. Starts at 1 (0 is reserved for "null").
    next_paw: AtomicU32,
}

impl Holt {
    /// Create a new empty Holt.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    /// let holt = Holt::new();
    /// ```
    pub fn new() -> Self {
        Self {
            stash: DashMap::new(),
            // Start at 1, reserving 0 for "null" / invalid paw
            next_paw: AtomicU32::new(1),
        }
    }

    /// Hold onto a resource - store it and get a paw grip.
    ///
    /// The resource is wrapped in an `Arc` and stored in the holt. You receive
    /// a unique [`Paw`] ID that can be used to retrieve the resource later.
    ///
    /// # Type Requirements
    ///
    /// The resource must be `'static + Send + Sync` - it must be safe to share
    /// across threads and have no non-static references.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// let data = vec![1, 2, 3];
    /// let paw = holt.hold(data);
    /// assert!(paw > 0);
    /// ```
    pub fn hold<T: Any + Send + Sync + 'static>(&self, resource: T) -> Paw {
        let paw = self.next_paw.fetch_add(1, Ordering::Relaxed);
        self.stash.insert(paw, Arc::new(resource));
        paw
    }

    /// Hold onto a pre-wrapped Arc resource.
    ///
    /// Use this when you already have an `Arc` and want to avoid double-wrapping.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    /// use std::sync::Arc;
    ///
    /// let holt = Holt::new();
    /// let data = Arc::new(vec![1, 2, 3]);
    /// let paw = holt.hold_arc(data);
    /// ```
    pub fn hold_arc<T: Any + Send + Sync + 'static>(&self, resource: Arc<T>) -> Paw {
        let paw = self.next_paw.fetch_add(1, Ordering::Relaxed);
        self.stash.insert(paw, resource);
        paw
    }

    /// Catch a resource by its paw - retrieve it from the holt.
    ///
    /// Returns a cloned `Arc` pointing to the resource. The resource remains
    /// in the holt until explicitly released.
    ///
    /// # Errors
    ///
    /// - [`HoltError::SlippedAway`] - The paw doesn't exist in the holt
    /// - [`HoltError::WrongCatch`] - The resource exists but is a different type
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// let paw = holt.hold(vec![1, 2, 3]);
    ///
    /// let data = holt.catch::<Vec<i32>>(paw).unwrap();
    /// assert_eq!(*data, vec![1, 2, 3]);
    /// ```
    pub fn catch<T: Any + Send + Sync + 'static>(&self, paw: Paw) -> HoltResult<Arc<T>> {
        let entry = self
            .stash
            .get(&paw)
            .ok_or(HoltError::SlippedAway(paw))?;

        entry
            .value()
            .clone()
            .downcast::<T>()
            .map_err(|_| HoltError::WrongCatch(paw))
    }

    /// Try to catch a resource, returning None if not found or wrong type.
    ///
    /// This is a convenience method that converts errors to `None`.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// let paw = holt.hold("hello".to_string());
    ///
    /// // Correct type - Some
    /// assert!(holt.try_catch::<String>(paw).is_some());
    ///
    /// // Wrong type - None
    /// assert!(holt.try_catch::<i32>(paw).is_none());
    ///
    /// // Invalid paw - None
    /// assert!(holt.try_catch::<String>(999).is_none());
    /// ```
    pub fn try_catch<T: Any + Send + Sync + 'static>(&self, paw: Paw) -> Option<Arc<T>> {
        self.catch(paw).ok()
    }

    /// Release a resource - remove it from the holt.
    ///
    /// Returns the resource if it existed, or `None` if the paw was invalid.
    /// Note that the resource may not be immediately dropped if there are
    /// outstanding `Arc` references from previous `catch` calls.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// let paw = holt.hold("hello".to_string());
    ///
    /// // Release the resource
    /// let released = holt.release(paw);
    /// assert!(released.is_some());
    ///
    /// // Can't catch it anymore
    /// assert!(holt.catch::<String>(paw).is_err());
    /// ```
    pub fn release(&self, paw: Paw) -> Option<Arc<dyn Any + Send + Sync>> {
        self.stash.remove(&paw).map(|(_, v)| v)
    }

    /// Release a resource and downcast it to a specific type.
    ///
    /// Combines `release` and downcasting in one operation.
    ///
    /// # Errors
    ///
    /// - [`HoltError::SlippedAway`] - The paw doesn't exist
    /// - [`HoltError::WrongCatch`] - Type mismatch
    pub fn release_as<T: Any + Send + Sync + 'static>(&self, paw: Paw) -> HoltResult<Arc<T>> {
        let removed = self
            .stash
            .remove(&paw)
            .ok_or(HoltError::SlippedAway(paw))?;

        removed
            .1
            .downcast::<T>()
            .map_err(|_| HoltError::WrongCatch(paw))
    }

    /// Check if a paw exists in the holt.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// let paw = holt.hold(42);
    ///
    /// assert!(holt.has(paw));
    /// assert!(!holt.has(999));
    /// ```
    pub fn has(&self, paw: Paw) -> bool {
        self.stash.contains_key(&paw)
    }

    /// Get the number of resources currently stored.
    ///
    /// # Example
    ///
    /// ```
    /// use otter_runtime::holt::Holt;
    ///
    /// let holt = Holt::new();
    /// assert_eq!(holt.len(), 0);
    ///
    /// holt.hold(1);
    /// holt.hold(2);
    /// assert_eq!(holt.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        self.stash.len()
    }

    /// Check if the holt is empty.
    pub fn is_empty(&self) -> bool {
        self.stash.is_empty()
    }

    /// Clear all resources from the holt.
    ///
    /// This releases all resources. Outstanding `Arc` references will still
    /// keep the resources alive until dropped.
    pub fn clear(&self) {
        self.stash.clear();
    }
}

impl Default for Holt {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Holt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Holt")
            .field("count", &self.stash.len())
            .field("next_paw", &self.next_paw.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hold_and_catch() {
        let holt = Holt::new();

        let paw = holt.hold(vec![1, 2, 3]);
        assert!(paw > 0);

        let data = holt.catch::<Vec<i32>>(paw).unwrap();
        assert_eq!(*data, vec![1, 2, 3]);
    }

    #[test]
    fn test_wrong_type() {
        let holt = Holt::new();

        let paw = holt.hold("hello".to_string());

        let result = holt.catch::<i32>(paw);
        assert!(matches!(result, Err(HoltError::WrongCatch(_))));
    }

    #[test]
    fn test_slipped_away() {
        let holt = Holt::new();

        let result = holt.catch::<i32>(999);
        assert!(matches!(result, Err(HoltError::SlippedAway(999))));
    }

    #[test]
    fn test_release() {
        let holt = Holt::new();

        let paw = holt.hold(42);
        assert!(holt.has(paw));

        let released = holt.release(paw);
        assert!(released.is_some());
        assert!(!holt.has(paw));

        // Can't catch after release
        assert!(holt.catch::<i32>(paw).is_err());
    }

    #[test]
    fn test_release_as() {
        let holt = Holt::new();

        let paw = holt.hold(42i32);
        let value = holt.release_as::<i32>(paw).unwrap();
        assert_eq!(*value, 42);

        // Paw is now invalid
        assert!(!holt.has(paw));
    }

    #[test]
    fn test_hold_arc() {
        let holt = Holt::new();

        let data = Arc::new(vec![1, 2, 3]);
        let paw = holt.hold_arc(data.clone());

        let caught = holt.catch::<Vec<i32>>(paw).unwrap();
        assert_eq!(*caught, vec![1, 2, 3]);
        assert!(Arc::ptr_eq(&data, &caught));
    }

    #[test]
    fn test_multiple_catch() {
        let holt = Holt::new();

        let paw = holt.hold(vec![1, 2, 3]);

        // Multiple catches should work
        let a = holt.catch::<Vec<i32>>(paw).unwrap();
        let b = holt.catch::<Vec<i32>>(paw).unwrap();

        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn test_try_catch() {
        let holt = Holt::new();

        let paw = holt.hold("hello".to_string());

        assert!(holt.try_catch::<String>(paw).is_some());
        assert!(holt.try_catch::<i32>(paw).is_none());
        assert!(holt.try_catch::<String>(999).is_none());
    }

    #[test]
    fn test_len_and_clear() {
        let holt = Holt::new();

        assert!(holt.is_empty());
        assert_eq!(holt.len(), 0);

        holt.hold(1);
        holt.hold(2);
        holt.hold(3);

        assert_eq!(holt.len(), 3);

        holt.clear();
        assert!(holt.is_empty());
    }

    #[test]
    fn test_paw_never_zero() {
        let holt = Holt::new();

        // First paw should be 1, not 0
        let paw = holt.hold(42);
        assert_eq!(paw, 1);

        let paw2 = holt.hold(43);
        assert_eq!(paw2, 2);
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let holt = Arc::new(Holt::new());
        let mut handles = vec![];

        // Spawn multiple threads that hold and catch resources
        for i in 0..10 {
            let holt = holt.clone();
            handles.push(thread::spawn(move || {
                let paw = holt.hold(i);
                let value = holt.catch::<i32>(paw).unwrap();
                assert_eq!(*value, i);
                paw
            }));
        }

        let paws: Vec<Paw> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All paws should be unique
        let mut sorted = paws.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 10);
    }
}
