//! Streaming Chrome DevTools `.heapsnapshot` writer.
//!
//! The output is accepted by Chrome DevTools' "Memory" panel. The format is
//! documented at <https://developer.chrome.com/docs/devtools/memory-problems/heap-snapshots>;
//! the on-the-wire schema lives in V8's
//! `src/profiler/heap-snapshot-generator.cc`.
//!
//! # Contents
//! - [`write_heap_snapshot`] — walk the heap and stream one complete JSON
//!   document to a caller-owned writer.
//!
//! # Invariants
//! - The writer never materializes the JSON document, node table, or edge
//!   table. Its only size-dependent scratch is a sorted `u32` offset index,
//!   capped at [`HEAP_SNAPSHOT_INDEX_BYTE_LIMIT`].
//! - Snapshot traversal runs under the collector's single-mutator
//!   stop-the-world-equivalent contract. It never allocates in the GC heap.
//! - A child edge is emitted only when its compressed offset names a live
//!   object in the same snapshot index.
//! - Writer failures and scratch admission failures abort with an I/O error;
//!   no partial document is presented as successful output.
//!
//! # See also
//! - [`crate::snapshot`] — the in-memory graph used for retained-size tests.
//! - [`crate::census`] — bounded type/space summaries that need no edge graph.

use std::io::{self, BufWriter, Write};

use crate::compressed::RawGc;
use crate::heap::GcHeap;

/// Maximum scratch bytes retained for the live-object offset index.
pub const HEAP_SNAPSHOT_INDEX_BYTE_LIMIT: usize = 64 * 1024 * 1024;

const PREFIX: &[u8] = br#"{"snapshot":{"meta":{"node_fields":["type","name","id","self_size","edge_count","trace_node_id","detachedness"],"node_types":[["hidden","array","string","object","code","closure","regexp","number","native","synthetic","concatenated string","sliced string","symbol","bigint","object shape"],"string","number","number","number","number","number"],"edge_fields":["type","name_or_index","to_node"],"edge_types":[["context","element","property","internal","hidden","shortcut","weak"],"string_or_number","node"],"trace_function_info_fields":["function_id","name","script_name","script_id","line","column"]},"node_count":"#;

/// Walk the heap and stream a Chrome-compatible `.heapsnapshot` document.
///
/// The output arrays can be much larger than the heap itself, so they are
/// serialized directly. A compact sorted object-offset index is retained to
/// translate compressed child handles into the flat node-array offsets Chrome
/// expects.
///
/// # Errors
/// Returns writer failures, allocation failures, or a bounded-scratch error
/// when the live-object index would exceed
/// [`HEAP_SNAPSHOT_INDEX_BYTE_LIMIT`].
pub fn write_heap_snapshot<W: Write>(heap: &GcHeap, writer: &mut W) -> io::Result<()> {
    let (object_count, used_tags) = count_objects(heap)?;
    let index_capacity = checked_index_capacity(object_count)?;
    let mut offsets = Vec::new();
    offsets
        .try_reserve_exact(index_capacity)
        .map_err(|error| io::Error::other(format!("heap snapshot index allocation: {error}")))?;
    // SAFETY: `&GcHeap` while no allocator path is open is the documented
    // single-mutator STW-equivalent contract. The callback only copies offsets.
    unsafe {
        heap.for_each_live_object(|header| offsets.push(header_to_raw(header)));
    }
    if offsets.len() != index_capacity {
        return Err(io::Error::other(
            "heap changed while its snapshot index was being built",
        ));
    }
    offsets.sort_unstable();
    if offsets.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(io::Error::other(
            "heap snapshot index contains duplicate object offsets",
        ));
    }

    let edge_count = count_edges(heap, &offsets)?;
    let node_count = object_count
        .checked_add(1)
        .ok_or_else(|| io::Error::other("heap snapshot node count overflow"))?;
    let tag_names = tag_name_indices(&used_tags);

    let mut out = BufWriter::new(writer);
    out.write_all(PREFIX)?;
    write!(
        out,
        "{node_count},\"edge_count\":{edge_count},\"trace_function_count\":0}},\"nodes\":["
    )?;

    let mut first = true;
    write_node(&mut out, &mut first, [9, 1, 0, 0, 0, 0, 0])?;
    for (index, &offset) in offsets.iter().enumerate() {
        let header = raw_to_header(offset);
        // SAFETY: every offset came from the live-object walk above and the
        // single-mutator borrow keeps the object in place for the full export.
        let (tag, size) = unsafe { ((*header).type_tag(), (*header).size_bytes()) };
        let outgoing = object_edge_count(heap, header, &offsets)?;
        let id = u64::try_from(index + 1)
            .map_err(|_| io::Error::other("heap snapshot node id overflow"))?;
        write_node(
            &mut out,
            &mut first,
            [
                3,
                u64::from(tag_names[tag as usize]),
                id,
                u64::from(size),
                outgoing,
                0,
                0,
            ],
        )?;
    }

    out.write_all(b"],\"edges\":[")?;
    write_edges(&mut out, heap, &offsets)?;
    out.write_all(b"],\"strings\":[\"\",\"(GC roots)\"")?;
    for (tag, used) in used_tags.iter().enumerate() {
        if *used {
            write!(out, ",\"Object#{tag:02x}\"")?;
        }
    }
    out.write_all(b"]}")?;
    out.flush()
}

fn count_objects(heap: &GcHeap) -> io::Result<(u64, [bool; 256])> {
    let mut count = 0_u64;
    let mut overflowed = false;
    let mut used_tags = [false; 256];
    // SAFETY: same STW-equivalent contract as [`write_heap_snapshot`]. The
    // callback updates fixed-size native scratch only.
    unsafe {
        heap.for_each_live_object(|header| {
            let Some(next) = count.checked_add(1) else {
                overflowed = true;
                return;
            };
            count = next;
            used_tags[(*header).type_tag() as usize] = true;
        });
    }
    if overflowed {
        return Err(io::Error::other("heap snapshot object count overflow"));
    }
    Ok((count, used_tags))
}

fn checked_index_capacity(object_count: u64) -> io::Result<usize> {
    let count = usize::try_from(object_count)
        .map_err(|_| io::Error::other("heap snapshot object count exceeds this platform"))?;
    let bytes = count
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| io::Error::other("heap snapshot index size overflow"))?;
    if bytes > HEAP_SNAPSHOT_INDEX_BYTE_LIMIT {
        return Err(io::Error::other(format!(
            "heap snapshot needs {bytes} index bytes; limit is {HEAP_SNAPSHOT_INDEX_BYTE_LIMIT}"
        )));
    }
    Ok(count)
}

fn tag_name_indices(used_tags: &[bool; 256]) -> [u32; 256] {
    let mut indices = [0; 256];
    let mut next = 2_u32;
    for (tag, used) in used_tags.iter().enumerate() {
        if *used {
            indices[tag] = next;
            next += 1;
        }
    }
    indices
}

fn count_edges(heap: &GcHeap, offsets: &[u32]) -> io::Result<u64> {
    let mut total = 0_u64;
    for &offset in offsets {
        let count = object_edge_count(heap, raw_to_header(offset), offsets)?;
        total = total
            .checked_add(count)
            .ok_or_else(|| io::Error::other("heap snapshot edge count overflow"))?;
    }
    Ok(total)
}

fn object_edge_count(
    heap: &GcHeap,
    header: *mut crate::header::GcHeader,
    offsets: &[u32],
) -> io::Result<u64> {
    let mut count = 0_u64;
    let mut overflowed = false;
    // SAFETY: `header` came from the live-object index and its registered trace
    // callback is invoked under the same STW-equivalent heap borrow.
    unsafe {
        heap.trace_one(header, &mut |slot: *mut RawGc| {
            let child = (*slot).0;
            if child != 0 && offsets.binary_search(&child).is_ok() {
                let Some(next) = count.checked_add(1) else {
                    overflowed = true;
                    return;
                };
                count = next;
            }
        });
    }
    if overflowed {
        Err(io::Error::other("heap snapshot edge count overflow"))
    } else {
        Ok(count)
    }
}

fn write_edges<W: Write>(out: &mut W, heap: &GcHeap, offsets: &[u32]) -> io::Result<()> {
    let mut first = true;
    let mut write_error = None;
    for &offset in offsets {
        if write_error.is_some() {
            break;
        }
        let header = raw_to_header(offset);
        // SAFETY: same live-header and STW-equivalent contract as
        // [`object_edge_count`]. The callback writes only to the host sink.
        unsafe {
            heap.trace_one(header, &mut |slot: *mut RawGc| {
                if write_error.is_some() {
                    return;
                }
                let child = (*slot).0;
                if child == 0 {
                    return;
                }
                let Ok(index) = offsets.binary_search(&child) else {
                    return;
                };
                let target = match u64::try_from(index + 1)
                    .ok()
                    .and_then(|id| id.checked_mul(7))
                {
                    Some(target) => target,
                    None => {
                        write_error = Some(io::Error::other("heap snapshot edge target overflow"));
                        return;
                    }
                };
                if let Err(error) = write_values(out, &mut first, [3, 0, target]) {
                    write_error = Some(error);
                }
            });
        }
    }
    match write_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn write_node<W: Write>(out: &mut W, first: &mut bool, fields: [u64; 7]) -> io::Result<()> {
    write_values(out, first, fields)
}

fn write_values<W: Write, const N: usize>(
    out: &mut W,
    first: &mut bool,
    values: [u64; N],
) -> io::Result<()> {
    for value in values {
        if !std::mem::replace(first, false) {
            out.write_all(b",")?;
        }
        write!(out, "{value}")?;
    }
    Ok(())
}

fn header_to_raw(header: *mut crate::header::GcHeader) -> u32 {
    let base_addr = crate::compressed::cage_base_addr();
    let addr = header as usize;
    debug_assert!(addr >= base_addr);
    (addr - base_addr) as u32
}

fn raw_to_header(offset: u32) -> *mut crate::header::GcHeader {
    let base = crate::compressed::cage_base();
    // SAFETY: callers only pass offsets collected from live headers during the
    // same single-mutator snapshot walk.
    unsafe { base.add(offset as usize) as *mut crate::header::GcHeader }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_capacity_accepts_the_boundary_and_rejects_one_more_object() {
        let objects = HEAP_SNAPSHOT_INDEX_BYTE_LIMIT / std::mem::size_of::<u32>();
        assert_eq!(checked_index_capacity(objects as u64).unwrap(), objects);
        assert!(checked_index_capacity(objects as u64 + 1).is_err());
    }
}
