//! Confined Unix storage backend for the compile cache.
//!
//! # Contents
//! - [`load`] — bounded descriptor-relative entry reads.
//! - [`store`] — locked quota maintenance and atomic publication.
//! - [`scan_and_prune`] — complete bounded reconciliation of canonical names.
//!
//! # Invariants
//! - The root and every opened cache object are owned by the effective user
//!   and grant no group/world permissions.
//! - Prefixes, entries, temporary files, renames, and unlinks are resolved
//!   relative to already-validated directory descriptors with `O_NOFOLLOW`.
//! - Every regular cache inode has exactly one link. Cache maintenance never
//!   writes metadata through an inode that may also be visible outside the
//!   private root.
//! - Opens are nonblocking, so a FIFO or other substituted special file cannot
//!   stall a runtime before its type is rejected.
//! - One nonblocking root lock covers a complete scan/prune and publication.
//!   An incomplete or over-budget scan never creates a temporary file and
//!   never publishes the requested entry.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{CStr, CString};
use std::fs::{DirBuilder, File};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, FlockArg, OFlag, open, openat, renameat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, geteuid, unlinkat};

use super::{
    COMPILE_CACHE_KEY_BYTES, CompileCacheKey, CompileCachePolicy, MAX_COMPILE_CACHE_ENTRY_BYTES,
    read_bounded,
};

const LOCK_FILE: &str = ".compile-cache.lock";
const TEMPORARY_CREATE_ATTEMPTS: usize = 32;

static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn load(root: &Path, key: &CompileCacheKey) -> Option<Vec<u8>> {
    let root = open_root(root, false)?;
    let (prefix, suffix) = split_key(key);
    let directory = open_prefix(&root, prefix, false).ok()??;
    let file = open_private_file(&directory, suffix.as_c_str())?;
    let size_hint = file.metadata().ok().map(|metadata| metadata.len());
    read_bounded(file, size_hint, MAX_COMPILE_CACHE_ENTRY_BYTES)
}

pub(super) fn store(
    root_path: &Path,
    policy: CompileCachePolicy,
    key: &CompileCacheKey,
    bytes: &[u8],
) {
    let Ok(stored_bytes) = u64::try_from(bytes.len()) else {
        return;
    };
    if bytes.len() > MAX_COMPILE_CACHE_ENTRY_BYTES
        || stored_bytes > policy.max_bytes
        || policy.max_entries == 0
    {
        return;
    }

    let Some(root) = open_root(root_path, true) else {
        return;
    };
    let Some(_lock) = RootLock::acquire(&root) else {
        return;
    };
    if scan_and_prune(&root, policy, key, stored_bytes).is_none() {
        return;
    }

    let (prefix, suffix) = split_key(key);
    let Ok(Some(directory)) = open_prefix(&root, prefix, true) else {
        return;
    };
    let Some(temporary) = TemporaryEntry::create(directory.as_fd(), key) else {
        return;
    };
    let _ = temporary.write_and_commit(bytes, suffix.as_c_str());
}

fn split_key(key: &CompileCacheKey) -> (&str, CString) {
    let (prefix, suffix) = key.as_str().split_at(2);
    let suffix = CString::new(suffix).expect("validated hex key has no NUL");
    (prefix, suffix)
}

fn open_root(path: &Path, create: bool) -> Option<OwnedFd> {
    let created = if create {
        create_private_root(path)?
    } else {
        false
    };
    let descriptor = open(path, directory_open_flags(), Mode::empty()).ok()?;
    if created {
        fchmod(&descriptor, Mode::S_IRWXU).ok()?;
    }
    validate_descriptor(&descriptor, SFlag::S_IFDIR).then_some(descriptor)
}

fn create_private_root(path: &Path) -> Option<bool> {
    match private_dir_builder(false).create(path) {
        Ok(()) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Some(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent()?;
            private_dir_builder(true).create(parent).ok()?;
            match private_dir_builder(false).create(path) {
                Ok(()) => Some(true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Some(false),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

fn private_dir_builder(recursive: bool) -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700).recursive(recursive);
    builder
}

fn open_prefix(root: &OwnedFd, prefix: &str, create: bool) -> Result<Option<OwnedFd>, ()> {
    match openat(root, prefix, directory_open_flags(), Mode::empty()) {
        Ok(descriptor) => {
            if validate_descriptor(&descriptor, SFlag::S_IFDIR) {
                Ok(Some(descriptor))
            } else {
                Err(())
            }
        }
        Err(Errno::ENOENT) if !create => Ok(None),
        Err(Errno::ENOENT) => {
            match mkdirat(root, prefix, Mode::S_IRWXU) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(_) => return Err(()),
            }
            let descriptor =
                openat(root, prefix, directory_open_flags(), Mode::empty()).map_err(|_| ())?;
            if validate_descriptor(&descriptor, SFlag::S_IFDIR) {
                Ok(Some(descriptor))
            } else {
                Err(())
            }
        }
        Err(_) => Err(()),
    }
}

fn directory_open_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK
}

fn file_open_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK
}

fn validate_descriptor(descriptor: &impl AsFd, expected: SFlag) -> bool {
    let Ok(metadata) = fstat(descriptor) else {
        return false;
    };
    validate_metadata(&metadata, expected)
}

fn validate_metadata(metadata: &FileStat, expected: SFlag) -> bool {
    let kind = SFlag::from_bits_truncate(metadata.st_mode) & SFlag::S_IFMT;
    kind == expected
        && metadata.st_uid == geteuid().as_raw()
        && metadata.st_mode & 0o077 == 0
        && (expected != SFlag::S_IFREG || metadata.st_nlink == 1)
}

fn open_private_file(directory: &impl AsFd, name: &CStr) -> Option<File> {
    let metadata = fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW).ok()?;
    if !validate_metadata(&metadata, SFlag::S_IFREG) {
        return None;
    }
    let descriptor = openat(directory, name, file_open_flags(), Mode::empty()).ok()?;
    if !validate_descriptor(&descriptor, SFlag::S_IFREG) {
        return None;
    }
    Some(File::from(descriptor))
}

struct RootLock {
    _descriptor: OwnedFd,
}

impl RootLock {
    fn acquire(root: &OwnedFd) -> Option<Self> {
        let descriptor = openat(
            root,
            LOCK_FILE,
            OFlag::O_RDWR
                | OFlag::O_CREAT
                | OFlag::O_CLOEXEC
                | OFlag::O_NOFOLLOW
                | OFlag::O_NONBLOCK,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .ok()?;
        if !validate_descriptor(&descriptor, SFlag::S_IFREG) {
            return None;
        }
        #[allow(deprecated)]
        nix::fcntl::flock(descriptor.as_raw_fd(), FlockArg::LockExclusiveNonblock).ok()?;
        Some(Self {
            _descriptor: descriptor,
        })
    }
}

struct TemporaryEntry<'directory> {
    directory: BorrowedFd<'directory>,
    name: CString,
    file: Option<File>,
    committed: bool,
}

impl<'directory> TemporaryEntry<'directory> {
    fn create(directory: BorrowedFd<'directory>, key: &CompileCacheKey) -> Option<Self> {
        for _ in 0..TEMPORARY_CREATE_ATTEMPTS {
            let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
            let name =
                CString::new(format!(".tmp-{}-{}-{id}", key.as_str(), std::process::id())).ok()?;
            let descriptor = match openat(
                directory,
                name.as_c_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_NONBLOCK,
                Mode::S_IRUSR | Mode::S_IWUSR,
            ) {
                Ok(descriptor) => descriptor,
                Err(Errno::EEXIST) => continue,
                Err(_) => return None,
            };
            if !validate_descriptor(&descriptor, SFlag::S_IFREG) {
                let _ = unlinkat(directory, name.as_c_str(), UnlinkatFlags::NoRemoveDir);
                return None;
            }
            return Some(Self {
                directory,
                name,
                file: Some(File::from(descriptor)),
                committed: false,
            });
        }
        None
    }

    fn write_and_commit(mut self, bytes: &[u8], destination: &CStr) -> Result<(), ()> {
        let file = self.file.as_mut().ok_or(())?;
        file.write_all(bytes).map_err(|_| ())?;
        drop(self.file.take());
        renameat(
            self.directory,
            self.name.as_c_str(),
            self.directory,
            destination,
        )
        .map_err(|_| ())?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TemporaryEntry<'_> {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed {
            let _ = unlinkat(
                self.directory,
                self.name.as_c_str(),
                UnlinkatFlags::NoRemoveDir,
            );
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CacheSize {
    bytes: u64,
    entries: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct CacheEntryCandidate {
    modified: SystemTime,
    prefix: u8,
    suffix: Box<str>,
    bytes: u64,
}

impl Ord for CacheEntryCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.modified
            .cmp(&other.modified)
            .then_with(|| self.prefix.cmp(&other.prefix))
            .then_with(|| self.suffix.cmp(&other.suffix))
            .then_with(|| self.bytes.cmp(&other.bytes))
    }
}

impl PartialOrd for CacheEntryCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn scan_and_prune(
    root: &OwnedFd,
    policy: CompileCachePolicy,
    destination: &CompileCacheKey,
    stored_bytes: u64,
) -> Option<CacheSize> {
    let (destination_prefix, destination_suffix) = destination.as_str().split_at(2);
    let scan_started = SystemTime::now();
    let mut inspected = 0_usize;
    let mut size = CacheSize {
        bytes: stored_bytes,
        entries: 1,
    };
    let mut retained: BinaryHeap<Reverse<CacheEntryCandidate>> = BinaryHeap::new();

    for prefix in 0_u16..=255 {
        let prefix_name = format!("{prefix:02x}");
        let directory = match open_prefix(root, &prefix_name, false) {
            Ok(Some(directory)) => directory,
            Ok(None) => continue,
            Err(()) => return None,
        };
        let mut directory = Dir::from_fd(directory).ok()?;
        let mut names = Vec::new();
        for entry in directory.iter() {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            inspected = inspected.checked_add(1)?;
            if inspected > policy.max_scan_entries {
                return None;
            }
            names.try_reserve(1).ok()?;
            names.push(name.to_owned());
        }

        for name in names {
            let bytes = name.as_bytes();
            if is_stale_temporary_name(bytes) {
                if !unlink_cache_name(&directory, name.as_c_str()) {
                    return None;
                }
                continue;
            }
            let Ok(suffix) = std::str::from_utf8(bytes) else {
                continue;
            };
            if !is_cache_entry_suffix(suffix) {
                continue;
            }

            let Some(file) = open_private_file(&directory, name.as_c_str()) else {
                if !unlink_cache_name(&directory, name.as_c_str()) {
                    return None;
                }
                continue;
            };
            let metadata = file.metadata().ok()?;
            let modified = bounded_modified_time(&metadata, scan_started);
            let bytes = metadata.len();
            drop(file);

            if prefix_name == destination_prefix && suffix == destination_suffix {
                continue;
            }
            size.bytes = size.bytes.checked_add(bytes)?;
            size.entries = size.entries.checked_add(1)?;
            retained.try_reserve(1).ok()?;
            retained.push(Reverse(CacheEntryCandidate {
                modified,
                prefix: u8::try_from(prefix).ok()?,
                suffix: suffix.into(),
                bytes,
            }));

            while size.bytes > policy.max_bytes || size.entries > policy.max_entries {
                let Reverse(oldest) = retained.pop()?;
                if !unlink_candidate(root, &oldest) {
                    return None;
                }
                size.bytes = size.bytes.saturating_sub(oldest.bytes);
                size.entries = size.entries.saturating_sub(1);
            }
        }
    }

    (size.bytes <= policy.max_bytes && size.entries <= policy.max_entries).then_some(size)
}

fn bounded_modified_time(metadata: &std::fs::Metadata, now: SystemTime) -> SystemTime {
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    if modified <= now {
        modified
    } else {
        UNIX_EPOCH
    }
}

fn unlink_candidate(root: &OwnedFd, candidate: &CacheEntryCandidate) -> bool {
    let prefix = format!("{:02x}", candidate.prefix);
    let directory = match open_prefix(root, &prefix, false) {
        Ok(Some(directory)) => directory,
        Ok(None) => return true,
        Err(()) => return false,
    };
    let Ok(name) = CString::new(candidate.suffix.as_ref()) else {
        return false;
    };
    unlink_cache_name(&directory, name.as_c_str())
}

fn unlink_cache_name(directory: &impl AsFd, name: &CStr) -> bool {
    match unlinkat(directory, name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => true,
        Err(_) => false,
    }
}

fn is_cache_entry_suffix(value: &str) -> bool {
    value.len() == COMPILE_CACHE_KEY_BYTES - 2 && is_lower_hex(value.as_bytes())
}

fn is_stale_temporary_name(value: &[u8]) -> bool {
    let Some(rest) = value.strip_prefix(b".tmp-") else {
        return false;
    };
    let Some((key, ids)) = rest.split_at_checked(COMPILE_CACHE_KEY_BYTES) else {
        return false;
    };
    if !is_lower_hex(key) {
        return false;
    }
    let Some(ids) = ids.strip_prefix(b"-") else {
        return false;
    };
    let mut parts = ids.split(|byte| *byte == b'-');
    let Some(pid) = parts.next() else {
        return false;
    };
    let Some(id) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && is_canonical_decimal(pid)
        && is_canonical_decimal(id)
        && decimal_fits::<u32>(pid)
        && decimal_fits::<u64>(id)
}

fn is_lower_hex(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_canonical_decimal(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(u8::is_ascii_digit)
        && (value == b"0" || value.first() != Some(&b'0'))
}

fn decimal_fits<T: std::str::FromStr>(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<T>().ok())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{OpenOptions, Permissions};
    use std::io::{BufRead, BufReader, Read as _};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
    use std::process::{Command, Stdio};

    use otter_bytecode::BytecodeModule;
    use otter_syntax::SourceKind;

    use crate::compile_cache::{CompileCache, cache_key};

    fn compiled(source: &str) -> BytecodeModule {
        otter_compiler::compile_script_source_to_module(source, SourceKind::JavaScript, "<test>")
            .expect("compile fixture")
            .bytecode
    }

    fn key(source: &str) -> CompileCacheKey {
        cache_key(source, SourceKind::JavaScript, "<test>")
    }

    fn fixed_key(byte: char) -> CompileCacheKey {
        CompileCacheKey::try_from(byte.to_string().repeat(COMPILE_CACHE_KEY_BYTES).as_str())
            .unwrap()
    }

    fn private_directory(path: &Path) {
        private_dir_builder(true).create(path).unwrap();
        std::fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
    }

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn private_file(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn stored_module_round_trips_and_entries_are_owner_private() {
        let root = private_tempdir();
        let cache = CompileCache::new(root.path());
        let key = key("globalThis.x = 41 + 1;");
        let module = compiled("globalThis.x = 41 + 1;");

        assert!(cache.load(&key).is_none());
        cache.store(&key, &module);
        let restored = cache.load(&key).expect("stored module");
        assert_eq!(
            otter_bytecode::dump::to_json_pretty(restored.module()).unwrap(),
            otter_bytecode::dump::to_json_pretty(&module).unwrap()
        );
        assert_eq!(std::fs::metadata(root.path()).unwrap().mode() & 0o077, 0);
        assert_eq!(
            std::fs::metadata(cache.entry_path(&key).parent().unwrap())
                .unwrap()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(
            std::fs::metadata(cache.entry_path(&key)).unwrap().mode() & 0o077,
            0
        );
    }

    #[test]
    fn temporary_entries_are_unique_and_raii_cleaned() {
        let root = private_tempdir();
        let root_descriptor = open_root(root.path(), true).unwrap();
        let key = fixed_key('a');
        let (prefix, _) = split_key(&key);
        let directory = open_prefix(&root_descriptor, prefix, true)
            .unwrap()
            .unwrap();
        let first = TemporaryEntry::create(directory.as_fd(), &key).unwrap();
        let second = TemporaryEntry::create(directory.as_fd(), &key).unwrap();
        let first_path = root
            .path()
            .join(prefix)
            .join(first.name.to_string_lossy().as_ref());
        let second_path = root
            .path()
            .join(prefix)
            .join(second.name.to_string_lossy().as_ref());

        assert_ne!(first.name, second.name);
        assert!(first_path.exists());
        assert!(second_path.exists());
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn broad_or_unowned_shape_fails_closed() {
        let root = private_tempdir();
        std::fs::set_permissions(root.path(), Permissions::from_mode(0o755)).unwrap();
        let cache = CompileCache::new(root.path());
        let key = key("1 + 1");
        cache.store(&key, &compiled("1 + 1"));
        assert!(cache.load(&key).is_none());
        assert!(!cache.entry_path(&key).exists());
    }

    #[test]
    fn root_symlink_is_a_miss_and_never_mutates_its_target() {
        let parent = private_tempdir();
        let outside = private_tempdir();
        let root = parent.path().join("compiled");
        symlink(outside.path(), &root).unwrap();
        let cache = CompileCache::new(&root);
        let key = key("root symlink");

        cache.store(&key, &compiled("1 + 1"));

        assert!(cache.load(&key).is_none());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
        assert!(
            std::fs::symlink_metadata(root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn hardlinked_root_lock_rejects_store_without_mutating_the_shared_inode() {
        let root = private_tempdir();
        let outside = private_tempdir();
        let outside_lock = outside.path().join("shared-lock");
        private_file(&outside_lock, b"outside lock");
        let lock_path = root.path().join(LOCK_FILE);
        std::fs::hard_link(&outside_lock, &lock_path).unwrap();
        let before = std::fs::metadata(&outside_lock).unwrap();
        let cache = CompileCache::new(root.path());
        let key = key("hardlinked lock");

        cache.store(&key, &compiled("1 + 1"));

        assert!(cache.load(&key).is_none());
        assert_eq!(std::fs::read(&outside_lock).unwrap(), b"outside lock");
        let after = std::fs::metadata(&outside_lock).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(after.nlink(), 2);
    }

    #[test]
    fn quota_is_reconciled_for_every_store_and_independent_instances() {
        let root = private_tempdir();
        let policy = CompileCachePolicy {
            max_bytes: u64::MAX,
            max_entries: 1,
            max_scan_entries: 32,
        };
        let first = CompileCache::with_policy(root.path(), policy);
        let second = CompileCache::with_policy(root.path(), policy);
        let first_key = key("first");
        let second_key = key("second");

        first.store(&first_key, &compiled("globalThis.x = 1"));
        second.store(&second_key, &compiled("globalThis.x = 2"));

        assert!(first.load(&first_key).is_none());
        assert!(second.load(&second_key).is_some());
    }

    #[test]
    fn byte_quota_rejects_before_publication() {
        let root = private_tempdir();
        let cache = CompileCache::with_policy(
            root.path(),
            CompileCachePolicy {
                max_bytes: 0,
                max_entries: 8,
                max_scan_entries: 32,
            },
        );
        let key = key("byte quota");
        cache.store(&key, &compiled("1 + 1"));
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn incomplete_bounded_scan_never_publishes() {
        let root = private_tempdir();
        let prefix = root.path().join("aa");
        private_directory(&prefix);
        private_file(&prefix.join("clutter-one"), b"one");
        private_file(&prefix.join("clutter-two"), b"two");
        let cache = CompileCache::with_policy(
            root.path(),
            CompileCachePolicy {
                max_bytes: u64::MAX,
                max_entries: 8,
                max_scan_entries: 1,
            },
        );
        let key = key("bounded scan");
        cache.store(&key, &compiled("1 + 1"));
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn stale_exact_temporary_is_cleaned_but_clutter_is_retained() {
        let root = private_tempdir();
        let prefix = root.path().join("aa");
        private_directory(&prefix);
        let stale_key = fixed_key('a');
        let stale = prefix.join(format!(".tmp-{}-123-456", stale_key.as_str()));
        let near_miss = prefix.join(format!(".tmp-{}-01-456", stale_key.as_str()));
        private_file(&stale, b"stale");
        private_file(&near_miss, b"keep");

        let cache = CompileCache::new(root.path());
        let key = key("cleanup trigger");
        cache.store(&key, &compiled("1 + 1"));

        assert!(!stale.exists());
        assert_eq!(std::fs::read(near_miss).unwrap(), b"keep");
        assert!(cache.load(&key).is_some());
    }

    #[test]
    fn held_cross_process_root_lock_makes_an_independent_store_a_no_op() {
        let root = private_tempdir();
        let first = CompileCache::new(root.path());
        let second = CompileCache::new(root.path());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "compile_cache::unix::tests::hold_root_lock_subprocess",
                "--ignored",
                "--nocapture",
            ])
            .env("OTTER_COMPILE_CACHE_LOCK_TEST_ROOT", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn lock holder");
        let mut child_output = BufReader::new(child.stdout.take().expect("child stdout"));
        loop {
            let mut line = String::new();
            let read = child_output.read_line(&mut line).expect("lock readiness");
            assert_ne!(read, 0, "lock-holder subprocess exited before readiness");
            if line.contains("OTTER_COMPILE_CACHE_LOCK_READY") {
                break;
            }
        }
        let key = key("locked");

        second.store(&key, &compiled("1 + 1"));
        assert!(first.load(&key).is_none());
        drop(child.stdin.take().expect("child stdin"));
        assert!(child.wait().expect("lock holder exits").success());
        second.store(&key, &compiled("1 + 1"));
        assert!(first.load(&key).is_some());
    }

    #[test]
    #[ignore = "subprocess helper invoked by the cross-process lock test"]
    fn hold_root_lock_subprocess() {
        let Some(root) = std::env::var_os("OTTER_COMPILE_CACHE_LOCK_TEST_ROOT") else {
            return;
        };
        let root_descriptor = open_root(Path::new(&root), true).expect("helper root");
        let _lock = RootLock::acquire(&root_descriptor).expect("helper lock");
        println!("OTTER_COMPILE_CACHE_LOCK_READY");
        std::io::stdout().flush().expect("flush readiness");
        let mut release = Vec::new();
        std::io::stdin()
            .read_to_end(&mut release)
            .expect("wait for parent release");
    }

    #[test]
    fn fifo_entry_is_a_nonblocking_cache_miss() {
        let root = private_tempdir();
        let cache = CompileCache::new(root.path());
        let key = fixed_key('a');
        let path = cache.entry_path(&key);
        private_directory(path.parent().unwrap());
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn entry_symlink_load_never_follows_outside_root() {
        let root = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(root.path());
        let key = fixed_key('a');
        let path = cache.entry_path(&key);
        private_directory(path.parent().unwrap());
        let target = outside.path().join("target");
        private_file(&target, b"not cache data");
        symlink(&target, &path).unwrap();

        assert!(cache.load(&key).is_none());
        assert_eq!(std::fs::read(target).unwrap(), b"not cache data");
    }

    #[test]
    fn store_replaces_entry_symlink_without_touching_target() {
        let root = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(root.path());
        let key = fixed_key('a');
        let path = cache.entry_path(&key);
        private_directory(path.parent().unwrap());
        let target = outside.path().join("target");
        private_file(&target, b"outside");
        symlink(&target, &path).unwrap();

        cache.store(&key, &compiled("1 + 1"));

        assert_eq!(std::fs::read(target).unwrap(), b"outside");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(cache.load(&key).is_some());
    }

    #[test]
    fn prune_unlinks_canonical_symlink_but_never_its_target() {
        let root = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let poisoned = fixed_key('a');
        let poisoned_path = CompileCache::new(root.path()).entry_path(&poisoned);
        private_directory(poisoned_path.parent().unwrap());
        let target = outside.path().join("target");
        private_file(&target, b"outside");
        symlink(&target, &poisoned_path).unwrap();

        let cache = CompileCache::new(root.path());
        let key = fixed_key('b');
        cache.store(&key, &compiled("1 + 1"));

        assert!(!poisoned_path.exists());
        assert_eq!(std::fs::read(target).unwrap(), b"outside");
        assert!(cache.load(&key).is_some());
    }

    #[test]
    fn prefix_symlink_aborts_prune_and_publication() {
        let root = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("aa")).unwrap();
        let cache = CompileCache::new(root.path());
        let key = fixed_key('b');

        cache.store(&key, &compiled("1 + 1"));

        assert!(cache.load(&key).is_none());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn future_mtime_is_an_eviction_disadvantage_without_metadata_mutation() {
        let root = private_tempdir();
        let future = fixed_key('a');
        let recent = fixed_key('b');
        for key in [&future, &recent] {
            let path = CompileCache::new(root.path()).entry_path(key);
            private_directory(path.parent().unwrap());
            private_file(&path, b"entry");
        }
        let future_path = CompileCache::new(root.path()).entry_path(&future);
        let future_time = SystemTime::now() + std::time::Duration::from_secs(86_400);
        File::options()
            .write(true)
            .open(&future_path)
            .unwrap()
            .set_modified(future_time)
            .unwrap();
        let cache = CompileCache::with_policy(
            root.path(),
            CompileCachePolicy {
                max_bytes: u64::MAX,
                max_entries: 2,
                max_scan_entries: 32,
            },
        );

        cache.store(&fixed_key('c'), &compiled("1 + 1"));

        assert!(!future_path.exists());
        assert!(cache.entry_path(&recent).exists());
    }

    #[test]
    fn outside_hardlink_is_rejected_without_mutating_its_inode() {
        let root = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(root.path());
        let linked_key = fixed_key('a');
        let linked_path = cache.entry_path(&linked_key);
        private_directory(linked_path.parent().unwrap());
        let outside_path = outside.path().join("shared-inode");
        private_file(&outside_path, b"outside bytes");
        let future = SystemTime::now() + std::time::Duration::from_secs(86_400);
        File::options()
            .write(true)
            .open(&outside_path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        std::fs::hard_link(&outside_path, &linked_path).unwrap();
        let before = std::fs::metadata(&outside_path)
            .unwrap()
            .modified()
            .unwrap();

        assert!(cache.load(&linked_key).is_none());
        let stored_key = fixed_key('b');
        cache.store(&stored_key, &compiled("1 + 1"));

        assert!(!linked_path.exists());
        assert_eq!(std::fs::read(&outside_path).unwrap(), b"outside bytes");
        assert_eq!(
            std::fs::metadata(&outside_path)
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert!(cache.load(&stored_key).is_some());
    }

    #[test]
    fn corrupt_and_oversized_entries_are_misses() {
        let root = private_tempdir();
        let cache = CompileCache::new(root.path());
        let corrupt = fixed_key('a');
        let corrupt_path = cache.entry_path(&corrupt);
        private_directory(corrupt_path.parent().unwrap());
        private_file(&corrupt_path, b"not bytecode");
        assert!(cache.load(&corrupt).is_none());

        let oversized = fixed_key('b');
        let oversized_path = cache.entry_path(&oversized);
        private_directory(oversized_path.parent().unwrap());
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(oversized_path)
            .unwrap();
        file.set_len(MAX_COMPILE_CACHE_ENTRY_BYTES as u64 + 1)
            .unwrap();
        assert!(cache.load(&oversized).is_none());
    }

    #[test]
    fn unwritable_cache_is_silent() {
        let cache = CompileCache::new("/proc/nonexistent-otter-cache");
        let key = key("unwritable");
        cache.store(&key, &compiled("1 + 1"));
        assert!(cache.load(&key).is_none());
    }
}
