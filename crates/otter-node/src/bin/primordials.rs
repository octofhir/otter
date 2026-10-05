//! Offline AST producer for the one Node compat primordial dispatcher.
//!
//! # Contents
//! - `generate` writes one bootstrap and its exact source coverage manifest.
//! - `check` verifies that the current bootstrap regenerates identically.
//!
//! # Invariants
//! This binary parses source only. It never creates an Otter runtime or runs
//! JavaScript. Normal Node builds do not enable its parser dependencies.
//!
//! # See also
//! - `primordials_codegen` owns binding validation and static lookup recipes.

#[path = "../primordials_codegen/mod.rs"]
mod primordials_codegen;

use std::{io::Write, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("expected generate or check")?;
    if command != "generate" && command != "check" {
        return Err(format!("unknown command {command}").into());
    }
    let mut repo = None;
    let mut bootstrap = None;
    let mut out = None;
    let mut manifest = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("every option needs a path")?;
        let destination = match flag.as_str() {
            "--repo" => &mut repo,
            "--bootstrap" => &mut bootstrap,
            "--out" => &mut out,
            "--manifest" => &mut manifest,
            _ => return Err(format!("unknown option {flag}").into()),
        };
        if destination.replace(PathBuf::from(value)).is_some() {
            return Err(format!("duplicate option {flag}").into());
        }
    }
    let repo = repo.ok_or("--repo is required")?;
    let bootstrap = bootstrap.ok_or("--bootstrap is required")?;
    let result = primordials_codegen::generate(&repo, &bootstrap)?;
    match command.as_str() {
        "generate" => {
            let out = out.ok_or("generate requires --out")?;
            let manifest = manifest.ok_or("generate requires --manifest")?;
            if out == bootstrap || out == manifest || manifest == bootstrap {
                return Err("producer input/output/manifest paths must differ".into());
            }
            let record = result.manifest_text()?;
            // Existing input/output files, including symlinks to the input,
            // cannot be overwritten by the offline producer.
            let mut source = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)?;
            let mut manifest = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(manifest)?;
            source.write_all(result.source.as_bytes())?;
            manifest.write_all(record.as_bytes())?;
        }
        "check" => {
            if out.is_some() || manifest.is_some() {
                return Err("check does not write outputs".into());
            }
            if result.source != std::fs::read_to_string(&bootstrap)? {
                return Err("current bootstrap differs from its AST-derived dispatch".into());
            }
            println!("{}", result.manifest_text()?);
        }
        _ => return Err(format!("unknown command {command}").into()),
    }
    Ok(())
}
