//! Executable memory shared by one compiler's code objects.
//!
//! Every code object a compiler publishes is placed in large reserved
//! regions at a cache-line granule, next to its neighbours, rather than in a
//! page-aligned mapping of its own. Page-aligned objects start in the same
//! instruction-cache sets and spread a program's code over one translation
//! entry per object; packed objects share pages and sets the way their size
//! dictates. Free space is kept by address and by size: a request takes the
//! smallest block that fits, and a returned object merges with the free
//! blocks around it.
//!
//! # Contents
//! - [`CodeMemory`] — the regions of one compiler and their free space.
//! - [`CodeAllocation`] — one code object's placed bytes; dropping it returns
//!   them.
//!
//! # Invariants
//! - Allocations are [`GRANULE`]-aligned, disjoint and never move. Their bytes
//!   are written once, before the allocation is handed out, and never change
//!   while it lives.
//! - On Apple AArch64 the regions are `MAP_JIT`: only the writing thread lifts
//!   its write protection, around its own copy, and synchronizes the
//!   instruction cache for those bytes before returning. Every other thread
//!   keeps executing the same pages.
//! - On other targets the regions are readable, writable and executable
//!   together; instruction caches there are coherent with data writes or are
//!   synchronized after each copy. On Apple x86-64, which runs translated,
//!   each copy re-applies its pages' protection, the event that retires stale
//!   translations of them.
//! - A page lies wholly inside a free block or holds placed bytes. Pages that
//!   become wholly free are returned to the operating system and taken back
//!   when an allocation reaches them again, so retained memory follows the
//!   live code.
//! - An allocation keeps its [`CodeMemory`] alive; regions are unmapped only
//!   after every allocation in them was returned.
//!
//! # See also
//! - [`crate::CompiledCode`] — the code object that owns one allocation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

/// Alignment and size step of every placed code object.
pub(crate) const GRANULE: usize = 64;

/// Virtual size of one region; a larger object gets a region of its own size.
const REGION_BYTES: usize = 32 << 20;

/// Executable regions shared by the code objects of one compiler.
pub struct CodeMemory {
    state: Mutex<State>,
}

impl std::fmt::Debug for CodeMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("CodeMemory")
            .field("regions", &state.regions.len())
            .finish()
    }
}

struct State {
    regions: Vec<Region>,
    page: usize,
}

/// What the operating system holds for one page of a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageState {
    /// Never handed out since the region was reserved.
    Fresh,
    /// Part of, or touched by, an allocation.
    Used,
    /// Wholly free and returned to the operating system.
    Released,
}

struct Region {
    base: *mut u8,
    len: usize,
    /// Free blocks: start offset to length.
    free: BTreeMap<usize, usize>,
    /// The same blocks as `(length, start)`, smallest first.
    by_size: BTreeSet<(usize, usize)>,
    pages: Vec<PageState>,
}

// SAFETY: the region pointers name process-private mappings this value owns;
// all access to the bookkeeping goes through the mutex.
unsafe impl Send for State {}

/// One placed code object. Dropping it returns its bytes to the free space.
pub struct CodeAllocation {
    memory: Arc<CodeMemory>,
    region: usize,
    offset: usize,
    reserved: usize,
    len: usize,
    ptr: *const u8,
}

// SAFETY: the bytes are immutable after placement and the bookkeeping the
// drop touches is behind the memory's mutex.
unsafe impl Send for CodeAllocation {}
// SAFETY: shared access only reads the immutable placed bytes.
unsafe impl Sync for CodeAllocation {}

impl std::fmt::Debug for CodeAllocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeAllocation")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .field("reserved", &self.reserved)
            .finish()
    }
}

impl CodeAllocation {
    /// Address of the first placed byte.
    #[must_use]
    pub fn ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Exact number of placed bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` for an allocation holding no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes this allocation keeps out of the free space: its length rounded
    /// up to the granule.
    #[must_use]
    pub fn reserved(&self) -> usize {
        self.reserved
    }

    /// The placed bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr..ptr + len` lies in a live mapping this allocation
        // keeps alive, and was fully written before the allocation existed.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for CodeAllocation {
    fn drop(&mut self) {
        let mut state = self.memory.lock();
        let page = state.page;
        state.regions[self.region].release(self.offset, self.reserved, page);
    }
}

impl CodeMemory {
    /// An empty memory; regions are reserved on first use.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                regions: Vec::new(),
                page: os::page_bytes(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Place `bytes` as one executable code object, or `None` when the
    /// operating system refuses executable memory.
    pub fn place(self: &Arc<Self>, bytes: &[u8]) -> Option<CodeAllocation> {
        let reserved = bytes.len().max(1).next_multiple_of(GRANULE);
        let (region, offset, ptr) = {
            let mut state = self.lock();
            let page = state.page;
            let fit = state
                .regions
                .iter()
                .enumerate()
                .filter_map(|(index, region)| {
                    region
                        .by_size
                        .range((reserved, 0)..)
                        .next()
                        .map(|&(len, start)| (len, index, start))
                })
                .min();
            let (index, start) = match fit {
                Some((_, index, start)) => (index, start),
                None => {
                    let len = REGION_BYTES.max(reserved.next_multiple_of(page));
                    let region = Region::reserve(len, page)?;
                    state.regions.push(region);
                    (state.regions.len() - 1, 0)
                }
            };
            let region = &mut state.regions[index];
            if !region.take(start, reserved, page) {
                return None;
            }
            // SAFETY: `start + reserved <= region.len`, inside the mapping.
            (index, start, unsafe { region.base.add(start) })
        };
        // SAFETY: the block was just taken from the free space, so no other
        // allocation and no other writer names these bytes.
        unsafe { os::write(ptr, bytes) };
        Some(CodeAllocation {
            memory: Arc::clone(self),
            region,
            offset,
            reserved,
            len: bytes.len(),
            ptr,
        })
    }

    /// Virtual bytes reserved across all regions.
    #[must_use]
    pub fn reserved_bytes(&self) -> usize {
        self.lock().regions.iter().map(|region| region.len).sum()
    }

    /// Bytes currently in the free space of every region.
    #[must_use]
    pub fn free_bytes(&self) -> usize {
        self.lock()
            .regions
            .iter()
            .map(|region| region.free.values().sum::<usize>())
            .sum()
    }

    /// Pages of every region currently returned to the operating system.
    #[cfg(test)]
    fn released_pages(&self) -> usize {
        self.lock()
            .regions
            .iter()
            .map(|region| {
                region
                    .pages
                    .iter()
                    .filter(|&&state| state == PageState::Released)
                    .count()
            })
            .sum()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        for region in &self.regions {
            // SAFETY: every allocation keeps the memory alive, so none is
            // left in this mapping when the state drops.
            unsafe { os::unmap(region.base, region.len) };
        }
    }
}

impl Region {
    fn reserve(len: usize, page: usize) -> Option<Self> {
        let base = os::map(len)?;
        let mut free = BTreeMap::new();
        free.insert(0, len);
        let mut by_size = BTreeSet::new();
        by_size.insert((len, 0));
        Some(Self {
            base,
            len,
            free,
            by_size,
            pages: vec![PageState::Fresh; len.div_ceil(page)],
        })
    }

    /// Take `len` bytes from the front of the free block at `start`, and take
    /// back from the operating system any page they reach.
    fn take(&mut self, start: usize, len: usize, page: usize) -> bool {
        let Some(block) = self.free.remove(&start) else {
            return false;
        };
        self.by_size.remove(&(block, start));
        if block > len {
            self.free.insert(start + len, block - len);
            self.by_size.insert((block - len, start + len));
        }
        let end = (start + len).div_ceil(page);
        let mut index = start / page;
        while index < end {
            let state = self.pages[index];
            if state == PageState::Used {
                index += 1;
                continue;
            }
            let from = index;
            while index < end && self.pages[index] == state {
                self.pages[index] = PageState::Used;
                index += 1;
            }
            // SAFETY: the pages lie inside this region's mapping.
            unsafe {
                os::commit(
                    self.base.add(from * page),
                    (index - from) * page,
                    state == PageState::Released,
                );
            }
        }
        true
    }

    /// Return `len` bytes at `start`, merge them with adjacent free blocks and
    /// release the pages the merged block now covers whole.
    fn release(&mut self, start: usize, len: usize, page: usize) {
        let (mut begin, mut end) = (start, start + len);
        if let Some((&before, &size)) = self.free.range(..start).next_back()
            && before + size == start
        {
            self.free.remove(&before);
            self.by_size.remove(&(size, before));
            begin = before;
        }
        if let Some(size) = self.free.remove(&end) {
            self.by_size.remove(&(size, end));
            end += size;
        }
        self.free.insert(begin, end - begin);
        self.by_size.insert((end - begin, begin));
        let last = end / page;
        let mut index = begin.div_ceil(page);
        while index < last {
            if self.pages[index] != PageState::Used {
                index += 1;
                continue;
            }
            let from = index;
            while index < last && self.pages[index] == PageState::Used {
                self.pages[index] = PageState::Released;
                index += 1;
            }
            // SAFETY: the pages lie inside the mapping and inside a free
            // block, so no allocation reads them.
            unsafe { os::release(self.base.add(from * page), (index - from) * page) };
        }
    }
}

#[cfg(unix)]
mod os {
    /// The granule pages are released at. A translated x86-64 process on
    /// Apple hardware is told 4 KiB while the hardware page it advises is
    /// 16 KiB, so Apple targets never go below that.
    pub(super) fn page_bytes() -> usize {
        // SAFETY: `sysconf` has no preconditions.
        let reported = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .ok()
            .filter(|&page| page.is_power_of_two() && page <= super::REGION_BYTES)
            .unwrap_or(16 * 1024);
        if cfg!(target_vendor = "apple") {
            reported.max(16 * 1024)
        } else {
            reported
        }
    }

    pub(super) fn map(len: usize) -> Option<*mut u8> {
        #[cfg(target_vendor = "apple")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
        #[cfg(not(target_vendor = "apple"))]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE;
        // SAFETY: an anonymous private mapping at a kernel-chosen address.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                flags,
                -1,
                0,
            )
        };
        (base != libc::MAP_FAILED).then_some(base.cast())
    }

    /// # Safety
    /// `base..base + len` is one whole mapping from [`map`] that nothing reads.
    pub(super) unsafe fn unmap(base: *mut u8, len: usize) {
        // SAFETY: the caller passes one whole owned mapping.
        unsafe { libc::munmap(base.cast(), len) };
    }

    /// Take back pages for an allocation; `released` pages were returned by
    /// [`release`], the others were never handed out.
    ///
    /// # Safety
    /// The range lies in a mapping from [`map`].
    pub(super) unsafe fn commit(start: *mut u8, len: usize, released: bool) {
        #[cfg(target_vendor = "apple")]
        if released {
            // SAFETY: the caller passes pages of an owned mapping.
            unsafe { libc::madvise(start.cast(), len, libc::MADV_FREE_REUSE) };
        }
        #[cfg(not(target_vendor = "apple"))]
        let _ = (start, len, released);
    }

    /// # Safety
    /// The range lies in a mapping from [`map`] and no allocation holds it.
    pub(super) unsafe fn release(start: *mut u8, len: usize) {
        #[cfg(target_vendor = "apple")]
        let advice = libc::MADV_FREE_REUSABLE;
        #[cfg(not(target_vendor = "apple"))]
        let advice = libc::MADV_DONTNEED;
        // SAFETY: the caller passes free pages of an owned mapping.
        unsafe { libc::madvise(start.cast(), len, advice) };
    }

    /// # Safety
    /// `dst..dst + bytes.len()` lies in a mapping from [`map`] and no other
    /// thread reads or writes it.
    pub(super) unsafe fn write(dst: *mut u8, bytes: &[u8]) {
        #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
        // SAFETY: lifting this thread's `MAP_JIT` write protection affects no
        // other thread; it is restored before any generated code can run here.
        unsafe {
            libc::pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            libc::pthread_jit_write_protect_np(1);
        }
        #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
        // SAFETY: the caller owns the destination range.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        // A translated x86-64 process discards stale translations of a page
        // when its protection is set, not when it is written: the written
        // pages are given the protection they already have.
        #[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
        {
            let page = page_bytes();
            let start = (dst as usize) & !(page - 1);
            let end = (dst as usize + bytes.len()).next_multiple_of(page);
            // SAFETY: the pages lie inside a mapping from [`map`], which is
            // readable, writable and executable throughout.
            unsafe {
                libc::mprotect(
                    start as *mut libc::c_void,
                    end - start,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                );
            }
        }
        // SAFETY: the range was just written and is part of a live mapping.
        let written = unsafe { std::slice::from_raw_parts(dst, bytes.len()) };
        dynasmrt::cache_control::synchronize_icache(written);
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
        VirtualFree,
    };

    pub(super) fn page_bytes() -> usize {
        4096
    }

    pub(super) fn map(len: usize) -> Option<*mut u8> {
        // SAFETY: a fresh reservation at a system-chosen address.
        let base =
            unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_EXECUTE_READWRITE) };
        (!base.is_null()).then_some(base.cast())
    }

    /// # Safety
    /// `base` is one whole reservation from [`map`] that nothing reads.
    pub(super) unsafe fn unmap(base: *mut u8, _len: usize) {
        // SAFETY: the caller passes one whole owned reservation.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }

    /// Commit pages for an allocation, fresh or released alike.
    ///
    /// # Safety
    /// The range lies in a reservation from [`map`].
    pub(super) unsafe fn commit(start: *mut u8, len: usize, _released: bool) {
        // SAFETY: committing pages of an owned reservation.
        unsafe { VirtualAlloc(start.cast(), len, MEM_COMMIT, PAGE_EXECUTE_READWRITE) };
    }

    /// # Safety
    /// The range lies in a reservation from [`map`] and no allocation holds it.
    pub(super) unsafe fn release(start: *mut u8, len: usize) {
        // SAFETY: decommitting free pages of an owned reservation.
        unsafe { VirtualFree(start.cast(), len, MEM_DECOMMIT) };
    }

    /// # Safety
    /// `dst..dst + bytes.len()` lies in committed pages of a reservation from
    /// [`map`] and no other thread reads or writes it.
    pub(super) unsafe fn write(dst: *mut u8, bytes: &[u8]) {
        // SAFETY: the caller owns the destination range.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len()) };
    }
}

#[cfg(test)]
#[path = "code_memory_tests.rs"]
mod tests;
