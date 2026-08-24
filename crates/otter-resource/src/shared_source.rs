//! Accounted, shareable UTF-8 source ownership.
//!
//! [`SharedSource`] ties one retained source allocation to its exact
//! [`ResourceClass::SourceModuleBytes`] charge. [`SharedSourceBuilder`] admits
//! incremental bytes before retaining them and validates UTF-8 only after the
//! complete stream has arrived.
//!
//! # Contents
//! - [`SharedSource`] is the cheap-to-clone proof-carrying source handle.
//! - [`SharedSourceBuilder`] incrementally builds one accounted source.
//! - [`SharedSourceError`] reports admission, allocation, I/O, and UTF-8
//!   failures.
//!
//! # Invariants
//! - The source text and its non-cloneable lease live in the same private
//!   `Arc` allocation and cannot be separated through the public API.
//! - Every live source allocation is charged exactly once, regardless of how
//!   many [`SharedSource`] handles refer to it.
//! - A builder's lease always equals its retained byte length. Capacity is
//!   admitted before bytes are appended, and failed allocation restores the
//!   previous charge.
//! - Dropping a partial builder, or failing I/O or UTF-8 validation through
//!   [`SharedSource::read_utf8`], releases the complete partial charge.
//!
//! # See also
//! - [`crate::ResourceAccount`] owns the shared resource ledger.
//! - [`crate::ResourceLease`] provides the non-cloneable RAII charge carried
//!   by each physical source allocation.

use std::collections::TryReserveError;
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::ops::Deref;
use std::str::Utf8Error;
use std::sync::Arc;

use crate::{ResourceAccount, ResourceClass, ResourceError, ResourceLease};

const READ_CHUNK_BYTES: usize = 16 * 1024;

/// A shareable UTF-8 source allocation with one exact resource charge.
///
/// Cloning this handle clones only its internal [`Arc`]. The source text and
/// its [`ResourceClass::SourceModuleBytes`] lease remain one indivisible
/// physical owner and are released together after the final handle is dropped.
#[derive(Clone)]
pub struct SharedSource {
    inner: Arc<SharedSourceInner>,
}

struct SharedSourceInner {
    // Field order is intentional: release the physical text before its charge.
    text: Box<str>,
    _lease: ResourceLease,
}

impl SharedSource {
    /// Admit an already-produced UTF-8 string and take ownership of it.
    ///
    /// The exact UTF-8 byte length is charged to
    /// [`ResourceClass::SourceModuleBytes`] before the source is published.
    /// Use [`SharedSource::read_utf8`] or [`SharedSourceBuilder`] when bytes are
    /// still arriving from an external producer.
    ///
    /// # Errors
    /// Returns [`SharedSourceError::Resource`] if the exact source length
    /// cannot be admitted, or [`SharedSourceError::LengthOverflow`] on a target
    /// whose addressable source length cannot be represented by the ledger.
    pub fn admit(account: &ResourceAccount, source: String) -> Result<Self, SharedSourceError> {
        let amount = source_amount(source.len())?;
        let lease = account.reserve_exact(ResourceClass::SourceModuleBytes, amount)?;
        Ok(Self::from_parts(source.into_boxed_str(), lease))
    }

    /// Read one complete UTF-8 source through a fixed-size stack buffer.
    ///
    /// Each successful read is admitted before it is copied into retained
    /// storage. Any I/O, allocation, resource, or UTF-8 error drops the private
    /// partial builder before this function returns, restoring current usage.
    ///
    /// # Errors
    /// Returns [`SharedSourceError`] when reading, allocation, resource
    /// admission, length conversion, or final UTF-8 validation fails.
    pub fn read_utf8<R: Read>(
        account: &ResourceAccount,
        mut reader: R,
    ) -> Result<Self, SharedSourceError> {
        let mut builder = SharedSourceBuilder::new(account);
        builder.read_from(&mut reader)?;
        builder.finish_utf8()
    }

    /// Return the UTF-8 byte length of this source.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.text.len()
    }

    /// Return whether this source is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.text.is_empty()
    }

    fn from_parts(text: Box<str>, lease: ResourceLease) -> Self {
        debug_assert_eq!(
            u64::try_from(text.len()).expect("supported target source lengths fit in u64"),
            lease.amount()
        );
        debug_assert_eq!(lease.class(), ResourceClass::SourceModuleBytes);
        Self {
            inner: Arc::new(SharedSourceInner {
                text,
                _lease: lease,
            }),
        }
    }
}

impl AsRef<str> for SharedSource {
    fn as_ref(&self) -> &str {
        &self.inner.text
    }
}

impl Deref for SharedSource {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl fmt::Debug for SharedSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedSource")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Incrementally constructs one accounted UTF-8 source.
///
/// Byte chunks may split a UTF-8 code point; validation happens once in
/// [`SharedSourceBuilder::finish_utf8`]. The builder itself is intentionally
/// not cloneable. Dropping it releases the charge for every byte appended so
/// far.
#[must_use = "dropping the builder releases its partial source charge"]
pub struct SharedSourceBuilder {
    bytes: Vec<u8>,
    lease: ResourceLease,
}

impl SharedSourceBuilder {
    /// Create an empty builder charged to `account`.
    pub fn new(account: &ResourceAccount) -> Self {
        let lease = account
            .reserve_exact(ResourceClass::SourceModuleBytes, 0)
            .expect("an exact zero-byte reservation cannot exceed a valid ledger limit");
        Self {
            bytes: Vec::new(),
            lease,
        }
    }

    /// Admit and append one byte chunk.
    ///
    /// The exact new total is admitted before retained capacity is allocated.
    /// If allocation fails, the lease is restored to the previous byte length.
    /// A resource rejection leaves both the existing bytes and charge intact,
    /// so the caller may handle the error or simply drop the builder.
    ///
    /// # Errors
    /// Returns [`SharedSourceError::Resource`] when the new exact total exceeds
    /// its resource limit, [`SharedSourceError::Allocation`] when retained
    /// capacity cannot be allocated, or [`SharedSourceError::LengthOverflow`]
    /// when the combined length is not representable.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), SharedSourceError> {
        if chunk.is_empty() {
            return Ok(());
        }

        let previous_amount = source_amount(self.bytes.len())?;
        let new_len = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(SharedSourceError::LengthOverflow)?;
        let new_amount = source_amount(new_len)?;

        self.lease.resize(new_amount)?;
        if let Err(error) = self.bytes.try_reserve_exact(chunk.len()) {
            self.lease
                .resize(previous_amount)
                .expect("shrinking to the previously admitted amount cannot fail");
            return Err(SharedSourceError::Allocation(error));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    /// Append all bytes read through a fixed-size stack buffer.
    ///
    /// An interrupted read is retried. Any other error leaves already-read
    /// bytes and their exact lease in this builder; dropping the builder rolls
    /// them back, while a caller that owns the reader may deliberately retry.
    ///
    /// # Errors
    /// Returns [`SharedSourceError`] for a non-interrupted I/O error or an
    /// admission/allocation failure while appending a completed read.
    pub fn read_from<R: Read + ?Sized>(&mut self, reader: &mut R) -> Result<(), SharedSourceError> {
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => self.push_bytes(&chunk[..read])?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(SharedSourceError::Io(error)),
            }
        }
    }

    /// Validate the complete byte stream and publish one shared source.
    ///
    /// # Errors
    /// Returns [`SharedSourceError::InvalidUtf8`] if the complete stream is not
    /// valid UTF-8. Both the bytes and their lease are dropped before the error
    /// reaches the caller.
    pub fn finish_utf8(self) -> Result<SharedSource, SharedSourceError> {
        let Self { bytes, lease } = self;
        match String::from_utf8(bytes) {
            Ok(text) => Ok(SharedSource::from_parts(text.into_boxed_str(), lease)),
            Err(error) => Err(SharedSourceError::InvalidUtf8(error.utf8_error())),
        }
    }

    /// Publish the raw byte stream with its charge intact. For transient
    /// non-UTF-8 inputs (data-module payloads) that stay accounted while a
    /// host transform derives the retained source from them.
    pub fn finish_bytes(self) -> AccountedBytes {
        let Self { bytes, lease } = self;
        AccountedBytes {
            bytes,
            _lease: lease,
        }
    }

    /// Return the number of bytes currently retained by this builder.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Return whether this builder has retained no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for SharedSourceBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedSourceBuilder")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl PartialEq for SharedSource {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for SharedSource {}

impl std::hash::Hash for SharedSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

/// Owned bytes whose exact `SourceModuleBytes` charge lives with them.
///
/// Dropping the value releases the charge with the bytes.
#[must_use = "dropping releases the byte charge"]
pub struct AccountedBytes {
    bytes: Vec<u8>,
    _lease: ResourceLease,
}

impl AccountedBytes {
    /// The accounted byte payload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::ops::Deref for AccountedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for AccountedBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountedBytes")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Failure to build or admit a [`SharedSource`].
#[derive(Debug)]
pub enum SharedSourceError {
    /// The exact source bytes could not be admitted by the resource ledger.
    Resource(ResourceError),
    /// A streaming source provider returned an I/O error.
    Io(io::Error),
    /// Retained byte capacity could not be allocated without panicking.
    Allocation(TryReserveError),
    /// The complete source byte stream was not valid UTF-8.
    InvalidUtf8(Utf8Error),
    /// The source byte length cannot be represented by this target or ledger.
    LengthOverflow,
}

impl fmt::Display for SharedSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(error) => write!(formatter, "source resource admission failed: {error}"),
            Self::Io(error) => write!(formatter, "failed to read source bytes: {error}"),
            Self::Allocation(error) => {
                write!(
                    formatter,
                    "failed to allocate retained source bytes: {error}"
                )
            }
            Self::InvalidUtf8(error) => write!(formatter, "source is not valid UTF-8: {error}"),
            Self::LengthOverflow => formatter.write_str("source byte length overflowed"),
        }
    }
}

impl Error for SharedSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Resource(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Allocation(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::LengthOverflow => None,
        }
    }
}

impl From<ResourceError> for SharedSourceError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

fn source_amount(len: usize) -> Result<u64, SharedSourceError> {
    u64::try_from(len).map_err(|_| SharedSourceError::LengthOverflow)
}
