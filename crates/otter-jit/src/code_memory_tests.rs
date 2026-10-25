use super::{CodeMemory, GRANULE};

#[test]
fn objects_are_packed_at_the_granule_and_hold_their_bytes() {
    let memory = CodeMemory::new();
    let a = memory.place(&[1, 2, 3]).unwrap();
    let b = memory.place(&[4; 100]).unwrap();
    let c = memory.place(&[5]).unwrap();
    assert_eq!(a.bytes(), &[1, 2, 3]);
    assert_eq!(b.bytes(), &[4; 100][..]);
    assert_eq!(c.bytes(), &[5]);
    assert_eq!(a.ptr() as usize % GRANULE, 0);
    // Neighbours share pages: each starts where the previous one's granules end.
    assert_eq!(b.ptr() as usize, a.ptr() as usize + GRANULE);
    assert_eq!(c.ptr() as usize, b.ptr() as usize + 2 * GRANULE);
    assert_eq!(b.reserved(), 2 * GRANULE);
    assert_eq!(memory.reserved_bytes(), super::REGION_BYTES);
}

#[test]
fn returned_space_merges_and_serves_the_smallest_fitting_request() {
    let memory = CodeMemory::new();
    let total = super::REGION_BYTES;
    let a = memory.place(&[0; GRANULE]).unwrap();
    let b = memory.place(&[0; 3 * GRANULE]).unwrap();
    let c = memory.place(&[0; GRANULE]).unwrap();
    let b_start = b.ptr();
    drop(b);
    // The hole between `a` and `c` fits a request of its size or less.
    let d = memory.place(&[7; 2 * GRANULE]).unwrap();
    assert_eq!(d.ptr(), b_start);
    drop((a, c, d));
    assert_eq!(memory.free_bytes(), total);
    // Everything merged back into one block: a request of the whole region fits.
    let whole = memory.place(&vec![0; total]).unwrap();
    assert_eq!(memory.reserved_bytes(), total);
    drop(whole);
}

#[test]
fn wholly_free_pages_are_released_and_taken_back() {
    let memory = CodeMemory::new();
    let page = super::os::page_bytes();
    let big = memory.place(&vec![9; 4 * page]).unwrap();
    let tail = memory.place(&[1]).unwrap();
    assert_eq!(memory.released_pages(), 0);
    drop(big);
    // The first four pages lie wholly inside the free block; the fifth still
    // holds `tail`.
    assert_eq!(memory.released_pages(), 4);
    let again = memory.place(&vec![3; page + 1]).unwrap();
    assert_eq!(again.bytes(), &vec![3; page + 1][..]);
    assert_eq!(memory.released_pages(), 2);
    drop((tail, again));
}

#[test]
fn an_object_larger_than_a_region_gets_a_region_of_its_own() {
    let memory = CodeMemory::new();
    let large = memory.place(&vec![2; super::REGION_BYTES + 1]).unwrap();
    assert!(memory.reserved_bytes() > super::REGION_BYTES);
    assert_eq!(large.len(), super::REGION_BYTES + 1);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn placed_code_runs() {
    // mov w0, #42 ; ret
    let code: [u8; 8] = [0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];
    let memory = CodeMemory::new();
    let filler = memory.place(&[0; 3]).unwrap();
    let placed = memory.place(&code).unwrap();
    dynasmrt::cache_control::prepare_for_execution(placed.bytes());
    // SAFETY: the placed bytes are a complete leaf function returning w0.
    let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(placed.ptr()) };
    assert_eq!(function(), 42);
    drop(filler);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn placed_code_runs() {
    // mov eax, 42 ; ret
    let code: [u8; 6] = [0xb8, 42, 0, 0, 0, 0xc3];
    let memory = CodeMemory::new();
    let filler = memory.place(&[0; 3]).unwrap();
    let placed = memory.place(&code).unwrap();
    // SAFETY: the placed bytes are a complete leaf function returning eax.
    let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(placed.ptr()) };
    assert_eq!(function(), 42);
    drop(filler);
}
