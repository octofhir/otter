//! Native matcher tier: hot programs run as machine code.
//!
//! A [`crate::Regex`] searched [`NATIVE_AFTER`] times compiles its program,
//! once per subject width, into a machine-code leftmost search over a window
//! of candidate starts. The start filter, every attempt, its backtracking and
//! its capture bookkeeping run in emitted code, which returns to Rust only
//! with a match, an exhausted window or an exhausted step budget.
//!
//! # Contents
//! - [`NativeTier`] — per-regex search counter and the code per width.
//! - [`NativeCode`] — the finalized code of one program at one width.
//! - [`Scratch`] — the backtrack stack, undo log and capture slots it runs on.
//! - [`Window`] / [`search`] — run one window of candidate starts.
//! - [`State`] — the block emitted code reads its inputs from.
//!
//! # Invariants
//! - A native search is observably the interpreter's: the same candidate
//!   starts in the same order, the same backtrack frames pushed and popped in
//!   the same order, and one step per popped frame (an attempt's entry
//!   included), so a step budget ends both at exactly the same point.
//! - Only programs the emitter covers compile: code-unit mode (no `u`/`v`)
//!   without lookaround. Every other program stays on the interpreter.
//! - The unit tests emitted code makes inline are tables built from the
//!   interpreter's own predicates; a unit no table covers calls back into
//!   those same predicates ([`HighPred`]).
//! - Emitted code embeds no pointer into the program, the regex or the
//!   subject. It reads all of them through [`State`], so a cloned or moved
//!   regex never leaves code referring to freed data.
//! - Capture and loop-mark slots hold [`NONE`] when unset.
//! - AArch64 and x86-64 have emitters; on any other target no program
//!   compiles and every search runs on the interpreter.
//!
//! # See also
//! - [`super::backtrack`] — the interpreter this tier mirrors.

#![cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(dead_code)
)]

#[cfg(target_arch = "aarch64")]
mod arm64;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod plan;
#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64")))]
mod tests;
#[cfg(target_arch = "x86_64")]
mod x86_64;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use super::backtrack::{char_eq, class_member};
use crate::casefold::canonicalize;
use crate::cursor::Unit;
use crate::program::{Insn, Program};

/// Searches a regex runs on the interpreter before its program compiles.
const NATIVE_AFTER: u32 = 8;

/// An unset capture or loop-mark slot.
pub(crate) const NONE: u64 = u64::MAX;

/// Words of one backtrack frame: resume address, position, undo-log mark and
/// an auxiliary word (a fused repeat's give-back bound).
const FRAME_WORDS: usize = 4;

/// Words of one undo-log entry: slot index and the value it held.
const LOG_WORDS: usize = 2;

/// Frames and undo entries a fresh [`Scratch`] holds before growing.
const INITIAL_ENTRIES: usize = 64;

/// Frames and undo entries a host keeps between searches; a search that grew
/// past this hands back empty buffers instead.
const RETAINED_ENTRIES: usize = 1024;

/// A regex's native tier: how often it has been searched, and its compiled
/// code for each subject width once it has been searched enough.
#[derive(Default)]
pub(crate) struct NativeTier {
    searches: AtomicU32,
    narrow: OnceLock<Option<NativeCode>>,
    wide: OnceLock<Option<NativeCode>>,
}

impl Clone for NativeTier {
    /// A clone starts cold: it compiles its own code when it gets hot.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl core::fmt::Debug for NativeTier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeTier")
            .field("searches", &self.searches.load(Ordering::Relaxed))
            .field("narrow", &self.narrow.get().map(Option::is_some))
            .field("wide", &self.wide.get().map(Option::is_some))
            .finish()
    }
}

impl NativeTier {
    /// The code for searching `U` subjects, compiling it once this regex has
    /// been searched often enough. `None` while cold, and for good when the
    /// emitter does not cover `program`.
    pub(crate) fn code<U: Unit>(&self, program: &Program) -> Option<&NativeCode> {
        let cell = if U::WIDE { &self.wide } else { &self.narrow };
        if let Some(code) = cell.get() {
            return code.as_ref();
        }
        if self.searches.fetch_add(1, Ordering::Relaxed) + 1 < NATIVE_AFTER {
            return None;
        }
        cell.get_or_init(|| compile::<U>(program)).as_ref()
    }
}

/// Compile `program` for `U` subjects, when the emitter covers it.
pub(crate) fn compile<U: Unit>(program: &Program) -> Option<NativeCode> {
    if !covers(program) {
        return None;
    }
    #[cfg(target_arch = "aarch64")]
    {
        arm64::compile(program, U::WIDE)
    }
    #[cfg(target_arch = "x86_64")]
    {
        x86_64::compile(program, U::WIDE)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = U::WIDE;
        None
    }
}

/// Whether the emitter covers `program`: code-unit mode, no lookaround, and
/// slot offsets within a load's immediate range.
fn covers(program: &Program) -> bool {
    !program.unicode
        && program.slot_count() <= 4000
        && !program
            .insns
            .iter()
            .any(|insn| matches!(insn, Insn::Look { .. } | Insn::LookMatch))
}

/// A unit test emitted code delegates for code units at or above `0x100`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HighPred {
    /// [`char_eq`] against a literal.
    Char { cp: u32, ignore_case: bool },
    /// [`class_member`] of a class of the program.
    Class {
        class: u32,
        negate: bool,
        ignore_case: bool,
    },
    /// The program's start prefilter.
    Start,
}

/// The finalized machine code of one program for one subject width.
pub(crate) struct NativeCode {
    buf: dynasmrt::ExecutableBuffer,
    /// The search entry, resolved once in the finalized buffer.
    entry: extern "C" fn(*mut State) -> u64,
    preds: Box<[HighPred]>,
    /// [`thread_tag`] of the thread that finalized the code. That thread
    /// needs no instruction barrier before running it: finalizing already
    /// synchronized its instruction stream.
    finalized_on: u64,
}

impl NativeCode {
    /// The finalized `buf` whose search entry is at `entry`.
    pub(super) fn new(
        buf: dynasmrt::ExecutableBuffer,
        entry: dynasmrt::AssemblyOffset,
        preds: Vec<HighPred>,
    ) -> Self {
        // SAFETY: the entry is a finalized `extern "C" fn(*mut State) -> u64`
        // the emitter placed at `entry`; the buffer lives as long as this
        // code and is never rewritten.
        let entry = unsafe {
            core::mem::transmute::<*const u8, extern "C" fn(*mut State) -> u64>(buf.ptr(entry))
        };
        Self {
            buf,
            entry,
            preds: preds.into_boxed_slice(),
            finalized_on: thread_tag(),
        }
    }

    /// Make the code safe to run on the calling thread: when another thread
    /// finalized it, discard any instructions this core fetched before that.
    pub(crate) fn prepare(&self) {
        if thread_tag() != self.finalized_on {
            dynasmrt::cache_control::prepare_for_execution(&self.buf);
        }
    }
}

impl core::fmt::Debug for NativeCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeCode")
            .field("bytes", &self.buf.len())
            .field("preds", &self.preds.len())
            .finish()
    }
}

/// Inputs and outputs of one native search, read and written by emitted code
/// at the offsets in [`offsets`].
#[repr(C)]
pub(crate) struct State {
    text: *const u8,
    len: u64,
    start: u64,
    window_end: u64,
    filter: u64,
    caps: *mut u64,
    stack_base: *mut u64,
    stack_limit: *mut u64,
    log_base: *mut u64,
    log_cap: u64,
    budget: u64,
    next: u64,
    spill_sp: u64,
    program: *const Program,
    preds: *const HighPred,
    stack: *mut Vec<u64>,
    log: *mut Vec<u64>,
}

/// Byte offsets of [`State`] fields.
pub(crate) mod offsets {
    use super::State;
    use core::mem::offset_of;

    pub(crate) const TEXT: u32 = offset_of!(State, text) as u32;
    pub(crate) const LEN: u32 = offset_of!(State, len) as u32;
    pub(crate) const START: u32 = offset_of!(State, start) as u32;
    pub(crate) const WINDOW_END: u32 = offset_of!(State, window_end) as u32;
    pub(crate) const FILTER: u32 = offset_of!(State, filter) as u32;
    pub(crate) const CAPS: u32 = offset_of!(State, caps) as u32;
    pub(crate) const STACK_BASE: u32 = offset_of!(State, stack_base) as u32;
    pub(crate) const STACK_LIMIT: u32 = offset_of!(State, stack_limit) as u32;
    pub(crate) const LOG_BASE: u32 = offset_of!(State, log_base) as u32;
    pub(crate) const LOG_CAP: u32 = offset_of!(State, log_cap) as u32;
    pub(crate) const BUDGET: u32 = offset_of!(State, budget) as u32;
    pub(crate) const NEXT: u32 = offset_of!(State, next) as u32;
    pub(crate) const SPILL_SP: u32 = offset_of!(State, spill_sp) as u32;
}

/// The buffers native code runs on, reused across searches.
///
/// Every buffer is kept at its full length so emitted code may write any word
/// of it; growth doubles the length and copies the words in use.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    stack: Vec<u64>,
    log: Vec<u64>,
    caps: Vec<u64>,
}

impl Scratch {
    /// Whether these buffers grew past what a host keeps between searches.
    pub(crate) fn is_oversized(&self) -> bool {
        self.stack.len() > RETAINED_ENTRIES * FRAME_WORDS
            || self.log.len() > RETAINED_ENTRIES * LOG_WORDS
    }

    /// The capture slots of the last match, [`NONE`] where unset.
    pub(crate) fn captures(&self) -> &[u64] {
        &self.caps
    }
}

/// The candidate starts of one native search.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    /// First candidate start.
    pub(crate) start: usize,
    /// Last candidate start, inclusive.
    pub(crate) end: usize,
    /// Whether a candidate below the subject's end is skipped unless its unit
    /// can begin a match (the program's first-set prefilter).
    pub(crate) filter: bool,
}

/// How a native search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A match; its slots are [`Scratch::captures`].
    Match,
    /// No candidate in the window matched; the search continues at this start.
    Exhausted(usize),
    /// The step budget ran out.
    StepLimit,
}

/// Run every candidate start of `window` over `text` until one matches.
/// `budget` is the number of steps the search may still take and comes back
/// reduced by the steps taken.
#[inline(always)]
pub(crate) fn search<U: Unit>(
    code: &NativeCode,
    program: &Program,
    text: &[U],
    window: Window,
    budget: &mut u64,
    scratch: &mut Scratch,
) -> Outcome {
    let slots = program.slot_count();
    if scratch.caps.len() < slots {
        scratch.caps.resize(slots, NONE);
    }
    if scratch.stack.len() < INITIAL_ENTRIES * FRAME_WORDS {
        scratch.stack.resize(INITIAL_ENTRIES * FRAME_WORDS, 0);
    }
    if scratch.log.len() < INITIAL_ENTRIES * LOG_WORDS {
        scratch.log.resize(INITIAL_ENTRIES * LOG_WORDS, 0);
    }
    let stack_base = scratch.stack.as_mut_ptr();
    let stack_words = scratch.stack.len();
    let mut state = State {
        text: text.as_ptr().cast(),
        len: text.len() as u64,
        start: window.start as u64,
        window_end: window.end as u64,
        filter: u64::from(window.filter),
        caps: scratch.caps.as_mut_ptr(),
        stack_base,
        stack_limit: stack_base.wrapping_add(stack_words - FRAME_WORDS),
        log_base: scratch.log.as_mut_ptr(),
        log_cap: (scratch.log.len() * 8) as u64,
        budget: *budget,
        next: 0,
        spill_sp: 0,
        program,
        preds: code.preds.as_ptr(),
        stack: &raw mut scratch.stack,
        log: &raw mut scratch.log,
    };
    // The entry was emitted for `program` at this width and prepared for
    // this thread when the search began; `code` outlives the call, and
    // every pointer in `state` stays valid for it: the buffers it grows are
    // re-read from `state` after each growth helper returns.
    let status = (code.entry)(&raw mut state);
    *budget = state.budget;
    match status {
        0 => Outcome::Exhausted(state.next as usize),
        1 => Outcome::Match,
        _ => Outcome::StepLimit,
    }
}

/// An identity of the calling thread: its thread-pointer register, which no
/// two live threads share.
#[cfg(target_arch = "aarch64")]
pub(crate) fn thread_tag() -> u64 {
    let tag: u64;
    // SAFETY: reading the thread-pointer register has no side effects.
    #[cfg(target_vendor = "apple")]
    unsafe {
        core::arch::asm!("mrs {}, tpidrro_el0", out(reg) tag, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: as above.
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) tag, options(nomem, nostack, preserves_flags));
    }
    tag
}

/// Every thread shares one tag where instruction fetch is coherent with
/// data writes and no barrier is ever needed.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn thread_tag() -> u64 {
    0
}

/// Double the backtrack stack, keeping the frames in use. Called by emitted
/// code with its stack top spilled to [`State::spill_sp`].
///
/// # Safety
/// `state` is the live state of a running search.
unsafe extern "C" fn grow_stack(state: *mut State) {
    // SAFETY: the caller passes the state of the running search, whose
    // `stack` points at the scratch vector that search borrows mutably.
    let (state, stack) = unsafe { (&mut *state, &mut *(*state).stack) };
    let used = (state.spill_sp as usize - state.stack_base as usize) / 8;
    let words = stack.len() * 2;
    stack.resize(words, 0);
    state.stack_base = stack.as_mut_ptr();
    state.stack_limit = state.stack_base.wrapping_add(words - FRAME_WORDS);
    state.spill_sp = state.stack_base.wrapping_add(used) as u64;
}

/// Double the undo log, keeping its entries.
///
/// # Safety
/// `state` is the live state of a running search.
unsafe extern "C" fn grow_log(state: *mut State) {
    // SAFETY: as for `grow_stack`, with the log vector.
    let (state, log) = unsafe { (&mut *state, &mut *(*state).log) };
    let words = log.len() * 2;
    log.resize(words, 0);
    state.log_base = log.as_mut_ptr();
    state.log_cap = (words * 8) as u64;
}

/// Whether code unit `unit` (at or above `0x100`) passes predicate `pred` of
/// the running code.
///
/// # Safety
/// `state` is the live state of a running search and `pred` indexes its
/// code's predicates.
unsafe extern "C" fn high_member(state: *const State, pred: u64, unit: u64) -> u64 {
    // SAFETY: the running search's program and predicate table outlive it.
    let (program, pred) = unsafe { (&*(*state).program, *(*state).preds.add(pred as usize)) };
    let unit = unit as u32;
    u64::from(match pred {
        HighPred::Char { cp, ignore_case } => char_eq(cp, unit, ignore_case, false),
        HighPred::Class {
            class,
            negate,
            ignore_case,
        } => class_member(
            &program.classes[class as usize],
            negate,
            unit,
            ignore_case,
            false,
        ),
        HighPred::Start => program
            .prefilter
            .as_ref()
            .is_some_and(|prefilter| prefilter.cp_may_start(unit)),
    })
}

/// A case-insensitive backreference to `start..end` at `pos`: the position
/// after it, or [`NONE`] when the subject does not hold the captured text
/// there.
///
/// # Safety
/// `state` is the live state of a running search over `U` units, and
/// `start < end` and `pos` lie within its subject.
unsafe extern "C" fn backref_ignore_case<U: Unit>(
    state: *const State,
    start: u64,
    end: u64,
    pos: u64,
) -> u64 {
    // SAFETY: the running search's subject is `len` units of `U`.
    let text =
        unsafe { core::slice::from_raw_parts((*state).text.cast::<U>(), (*state).len as usize) };
    let (start, len, pos) = (start as usize, (end - start) as usize, pos as usize);
    if pos + len > text.len() {
        return NONE;
    }
    let same = text[start..start + len]
        .iter()
        .zip(&text[pos..pos + len])
        .all(|(&a, &b)| {
            let (a, b): (u32, u32) = (a.into(), b.into());
            a == b || canonicalize(a) == canonicalize(b)
        });
    if same { (pos + len) as u64 } else { NONE }
}
