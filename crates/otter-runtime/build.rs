//! Build-time fingerprint for the bytecode compile-cache contract.
//!
//! # Contents
//! - Deterministic discovery and hashing of compiler/encoding sources.
//! - Target, profile, feature, cfg, rustflags, and toolchain inputs.
//! - `OTTER_COMPILER_CACHE_FINGERPRINT` emission for the runtime crate.
//!
//! # Invariants
//! - Discovered source paths are workspace-relative and sorted; their file
//!   metadata never enters the digest. Explicit compiler flags are hashed
//!   byte-for-byte because they may change conditional compilation.
//! - Only stable contents and explicit compile inputs are hashed. Timestamps,
//!   process ids, and the installed executable path are absent.
//! - Every traversed directory and file is registered with Cargo so additions,
//!   removals, and content changes rerun this script.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE_ROOTS: &[&str] = &[
    "crates/otter-syntax/src",
    "crates/otter-bytecode/src",
    "crates/otter-compiler/src",
    "crates/otter-regex/src",
    "crates/otter-regex/data",
];

const FIXED_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "crates/otter-syntax/Cargo.toml",
    "crates/otter-bytecode/Cargo.toml",
    "crates/otter-compiler/Cargo.toml",
    "crates/otter-regex/Cargo.toml",
    "crates/otter-regex/build.rs",
    "crates/otter-runtime/Cargo.toml",
    "crates/otter-runtime/build.rs",
    "crates/otter-runtime/src/compile_cache.rs",
    "crates/otter-runtime/src/compile_cache/unix.rs",
];

const EXPLICIT_ENV_INPUTS: &[&str] = &[
    "TARGET",
    "HOST",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "CARGO_ENCODED_RUSTFLAGS",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(required_env("CARGO_MANIFEST_DIR")?);
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("otter-runtime must live under <workspace>/crates")?;

    let mut files = Vec::new();
    for relative in FIXED_FILES {
        files.push(workspace.join(relative));
    }
    for relative in SOURCE_ROOTS {
        let root = workspace.join(relative);
        collect_regular_files(&root, &mut files)?;
    }
    files.sort_by(|left, right| {
        workspace_relative(workspace, left).cmp(&workspace_relative(workspace, right))
    });
    files.dedup();

    let mut hasher = blake3::Hasher::new();
    update_framed(&mut hasher, b"otter-compiler-cache-fingerprint")?;
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = workspace_relative(workspace, &path);
        update_framed(&mut hasher, b"file")?;
        update_framed(&mut hasher, relative.as_bytes())?;
        update_framed(&mut hasher, &fs::read(path)?)?;
    }

    let mut compile_inputs = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            // Unrelated Unix environment variables may contain arbitrary
            // bytes. They are not Cargo compiler inputs and must not make the
            // build script panic before it can filter them.
            continue;
        };
        if name.starts_with("CARGO_CFG_")
            || name.starts_with("CARGO_FEATURE_")
            || name.starts_with("CARGO_PROFILE_")
        {
            println!("cargo:rerun-if-env-changed={name}");
            let value = value
                .to_str()
                .ok_or_else(|| format!("Cargo compiler input {name} is not UTF-8"))?;
            compile_inputs.insert(name.to_string(), value.to_string());
        }
    }
    for name in EXPLICIT_ENV_INPUTS {
        println!("cargo:rerun-if-env-changed={name}");
        compile_inputs.insert((*name).to_string(), std::env::var(name).unwrap_or_default());
    }
    for (name, value) in compile_inputs {
        update_framed(&mut hasher, b"compile-env")?;
        update_framed(&mut hasher, name.as_bytes())?;
        update_framed(&mut hasher, value.as_bytes())?;
    }

    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = required_env("RUSTC")?;
    let version = Command::new(rustc).arg("-vV").output()?;
    if !version.status.success() {
        return Err("rustc -vV failed while computing cache fingerprint".into());
    }
    update_framed(&mut hasher, b"rustc-vV")?;
    update_framed(&mut hasher, &version.stdout)?;

    println!(
        "cargo:rustc-env=OTTER_COMPILER_CACHE_FINGERPRINT={}",
        hasher.finalize().to_hex()
    );
    Ok(())
}

fn required_env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("Cargo did not provide {name}").into())
}

fn collect_regular_files(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    // Cargo's directory watcher is not recursive on every supported release.
    // Register each visited directory so adding the first source below an
    // existing nested directory reruns fingerprint discovery.
    println!("cargo:rerun-if-changed={}", root.display());
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_regular_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        } else if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fingerprint input is a symlink: {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

fn workspace_relative(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .expect("fingerprint inputs stay under the workspace")
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .expect("repository paths are UTF-8")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn update_framed(hasher: &mut blake3::Hasher, bytes: &[u8]) -> io::Result<()> {
    let len = u64::try_from(bytes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "fingerprint component exceeds u64",
        )
    })?;
    hasher.update(&len.to_le_bytes());
    hasher.update(bytes);
    Ok(())
}
