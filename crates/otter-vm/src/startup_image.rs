//! The startup image a product executable carries in its own data section.
//!
//! A runtime build ends with a heap, code and side state that every run of the
//! same executable rebuilds identically. The product writes that state once,
//! after linking, into a section this module reserves in the executable
//! itself; a launch then restores it instead of bootstrapping. The image is
//! part of the signed executable, so it is exactly as trusted as the code that
//! reads it, and nothing is read from a writable cache.
//!
//! # Contents
//! - [`StartupImage`] — the image this executable carries, when one was
//!   written.
//! - [`write_into_executable`] — write an image into an executable file's
//!   reserved section.
//! - [`CAPACITY`] — bytes the section reserves.
//! - [`build_identity`] — the linker's identity of this executable, which an
//!   image is bound to.
//!
//! # Invariants
//! - The section starts with a marker no other bytes of an executable hold,
//!   then the image length, then the image. A length of zero means no image.
//! - A [`StartupImage`] exists only for bytes read from this executable's own
//!   section, so restoring from it trusts nothing outside the executable.
//! - The section is reached only through a pointer the optimizer cannot see
//!   through: its bytes change after linking, and no read may be folded to
//!   the zeros it was linked with.
//! - An image is bound to the build that wrote it through the linker's
//!   identity of the executable (the Mach-O UUID, the ELF GNU build id),
//!   which changes with the linked bytes and survives patching the section
//!   and re-signing. An executable without one carries no image.
//! - The marker literal is spelled only by the section itself: the scan
//!   compares against a masked copy, so no other constant of any build
//!   profile holds the marker bytes.
//!
//! # See also
//! - [`crate::snapshot::IsolateSnapshot::decode`] — what the image holds.

use std::cell::UnsafeCell;

/// Bytes the section reserves for an image.
pub const CAPACITY: usize = 8 << 20;

/// What the section starts with: written at link time, found by the writer.
const MARKER: [u8; 16] = *b"otter:startimg\0\x01";

/// The marker as the scan compares it, every byte flipped by [`MARKER_MASK`],
/// so the comparison's own constant never holds the marker bytes.
const MASKED_MARKER: [u8; 16] = {
    let mut masked = MARKER;
    let mut index = 0;
    while index < masked.len() {
        masked[index] ^= MARKER_MASK;
        index += 1;
    }
    masked
};

const MARKER_MASK: u8 = 0xa5;

#[repr(C, align(16))]
struct Section {
    marker: [u8; 16],
    len: [u8; 8],
    bytes: [u8; CAPACITY],
}

struct SectionCell(UnsafeCell<Section>);

// SAFETY: the section is written only by the post-link writer, before any
// process runs; at run time every access only reads it.
unsafe impl Sync for SectionCell {}

#[used]
#[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__otter_image"))]
#[cfg_attr(
    all(unix, not(target_vendor = "apple")),
    unsafe(link_section = ".otter_image")
)]
static SECTION: SectionCell = SectionCell(UnsafeCell::new(Section {
    marker: MARKER,
    len: [0; 8],
    bytes: [0; CAPACITY],
}));

/// The startup image this executable carries.
#[derive(Clone, Copy)]
pub struct StartupImage {
    bytes: &'static [u8],
}

impl StartupImage {
    /// The image written into this executable, or `None` when its section
    /// is still empty.
    #[must_use]
    pub fn of_this_executable() -> Option<Self> {
        let section = std::hint::black_box(SECTION.0.get());
        // SAFETY: the section lives for the process and nothing writes it
        // at run time; the volatile reads keep the length from being folded
        // to its link-time zero.
        let len = unsafe {
            u64::from_le_bytes(std::ptr::read_volatile(&raw const (*section).len)) as usize
        };
        if len == 0 || len > CAPACITY {
            return None;
        }
        // SAFETY: as above; `len` is within the reserved bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts((&raw const (*section).bytes).cast::<u8>(), len) };
        Some(Self { bytes })
    }

    /// A reader over the image's bytes.
    #[must_use]
    pub fn reader(&self) -> otter_gc::image_codec::ImageReader<'static> {
        otter_gc::image_codec::ImageReader::new(self.bytes)
    }

    /// The isolate snapshot the image holds after the caller read its own
    /// prefix from `input`, or `None` when the rest is not one.
    pub fn snapshot(
        &self,
        input: &mut otter_gc::image_codec::ImageReader<'static>,
        account: &otter_resource::ResourceAccount,
    ) -> Option<crate::snapshot::IsolateSnapshot> {
        // SAFETY: a `StartupImage` names only this executable's own section,
        // which its post-link writer filled from a capture of this build.
        let snapshot =
            unsafe { crate::snapshot::IsolateSnapshot::decode(input, &mut |_| None, account) }?;
        input.is_empty().then_some(snapshot)
    }

    /// Bytes the image occupies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the image is empty; a carried image never is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Write `image` into the reserved section of the executable file at `path`.
///
/// # Errors
/// Fails when the file cannot be read or written, holds no reserved section
/// (or more than one), or `image` exceeds [`CAPACITY`].
pub fn write_into_executable(path: &std::path::Path, image: &[u8]) -> std::io::Result<()> {
    if image.len() > CAPACITY {
        return Err(std::io::Error::other(format!(
            "startup image is {} bytes; the section holds {CAPACITY}",
            image.len()
        )));
    }
    let mut file = std::fs::read(path)?;
    let mut found = file
        .windows(MARKER.len())
        .enumerate()
        .filter(|(_, window)| {
            window
                .iter()
                .zip(MASKED_MARKER)
                .all(|(&byte, masked)| byte ^ MARKER_MASK == masked)
        })
        .map(|(offset, _)| offset);
    let (Some(offset), None) = (found.next(), found.next()) else {
        return Err(std::io::Error::other(
            "the executable does not hold exactly one startup-image section",
        ));
    };
    let len_at = offset + MARKER.len();
    let bytes_at = len_at + 8;
    if file.len() < bytes_at + CAPACITY {
        return Err(std::io::Error::other(
            "the startup-image section is truncated",
        ));
    }
    file[len_at..bytes_at].copy_from_slice(&(image.len() as u64).to_le_bytes());
    file[bytes_at..bytes_at + image.len()].copy_from_slice(image);
    file[bytes_at + image.len()..bytes_at + CAPACITY].fill(0);
    std::fs::write(path, file)
}

/// The linker's identity of this executable: the Mach-O `LC_UUID`, or the
/// ELF GNU build id's leading bytes. `None` when the executable carries none.
#[must_use]
pub fn build_identity() -> Option<[u8; 16]> {
    linker_identity()
}

#[cfg(target_vendor = "apple")]
fn linker_identity() -> Option<[u8; 16]> {
    #[repr(C)]
    struct MachHeader64 {
        magic: u32,
        cputype: i32,
        cpusubtype: i32,
        filetype: u32,
        ncmds: u32,
        sizeofcmds: u32,
        flags: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct LoadCommand {
        cmd: u32,
        cmdsize: u32,
    }
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    unsafe extern "C" {
        fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
    }
    // SAFETY: image 0 is the main executable, mapped for the life of the
    // process; its `ncmds` load commands follow the header back to back, each
    // `cmdsize` bytes long, and an `LC_UUID` command holds 16 bytes after its
    // two words.
    unsafe {
        let header = _dyld_get_image_header(0);
        if header.is_null() || (*header).magic != MH_MAGIC_64 {
            return None;
        }
        let mut command = header.add(1).cast::<u8>();
        for _ in 0..(*header).ncmds {
            let load = command.cast::<LoadCommand>();
            if (*load).cmd == LC_UUID {
                let mut uuid = [0u8; 16];
                std::ptr::copy_nonoverlapping(command.add(8), uuid.as_mut_ptr(), 16);
                return Some(uuid);
            }
            command = command.add((*load).cmdsize as usize);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linker_identity() -> Option<[u8; 16]> {
    const NT_GNU_BUILD_ID: u32 = 3;
    unsafe extern "C" fn first_object(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        found: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: the loader hands each loaded object's program headers, the
        // main executable first; a `PT_NOTE` segment holds 4-byte-aligned
        // notes (name size, descriptor size, type, name, descriptor).
        unsafe {
            let found = &mut *found.cast::<Option<[u8; 16]>>();
            let info = &*info;
            for index in 0..usize::from(info.dlpi_phnum) {
                let header = &*info.dlpi_phdr.add(index);
                if header.p_type != libc::PT_NOTE {
                    continue;
                }
                let mut note = (info.dlpi_addr as usize + header.p_vaddr as usize) as *const u8;
                let end = note.add(header.p_memsz as usize);
                while note.add(12) <= end {
                    let name_size = note.cast::<u32>().read_unaligned() as usize;
                    let desc_size = note.add(4).cast::<u32>().read_unaligned() as usize;
                    let kind = note.add(8).cast::<u32>().read_unaligned();
                    let name = note.add(12);
                    let desc = name.add(name_size.next_multiple_of(4));
                    if kind == NT_GNU_BUILD_ID
                        && name_size == 4
                        && std::slice::from_raw_parts(name, 4) == b"GNU\0"
                        && desc_size >= 16
                    {
                        let mut id = [0u8; 16];
                        std::ptr::copy_nonoverlapping(desc, id.as_mut_ptr(), 16);
                        *found = Some(id);
                        return 1;
                    }
                    note = desc.add(desc_size.next_multiple_of(4));
                }
            }
        }
        // Only the main executable names this build.
        1
    }
    let mut found: Option<[u8; 16]> = None;
    // SAFETY: the callback writes only through the pointer to `found`, which
    // outlives the iteration.
    unsafe {
        libc::dl_iterate_phdr(Some(first_object), (&raw mut found).cast());
    }
    found
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn linker_identity() -> Option<[u8; 16]> {
    None
}
