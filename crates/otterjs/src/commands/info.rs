//! Info command - show runtime and environment information.

use serde::Serialize;

/// Run the info command
pub fn run(json: bool) {
    let info = RuntimeInfo::collect();

    if json {
        if let Ok(s) = serde_json::to_string_pretty(&info) {
            println!("{}", s);
        }
    } else {
        print_human_readable(&info);
    }
}

fn print_human_readable(info: &RuntimeInfo) {
    println!("Otter Runtime");
    println!("=============");
    println!();
    println!("Version:     {}", info.version);
    println!("Platform:    {}", info.platform);
    println!("Arch:        {}", info.arch);
    println!();
    println!("Features:");
    println!(
        "  TypeScript:  {}",
        if info.features.typescript {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  Type Check:  {}",
        if info.features.type_check {
            "enabled (tsgo)"
        } else {
            "pending (being ported)"
        }
    );
    println!(
        "  ESM:         {}",
        if info.features.esm {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  VM Engine:   {}",
        if info.features.vm {
            "otter-vm-core"
        } else {
            "disabled"
        }
    );
    println!();
    println!("Paths:");
    if let Some(ref cwd) = info.paths.cwd {
        println!("  CWD:         {}", cwd);
    }
    if let Some(ref home) = info.paths.home {
        println!("  Home:        {}", home);
    }
    if let Some(ref cache) = info.paths.cache {
        println!("  Cache:       {}", cache);
    }
}

#[derive(Serialize)]
struct RuntimeInfo {
    version: String,
    platform: String,
    arch: String,
    features: Features,
    paths: Paths,
}

#[derive(Serialize)]
struct Features {
    typescript: bool,
    type_check: bool,
    esm: bool,
    vm: bool,
}

#[derive(Serialize)]
struct Paths {
    cwd: Option<String>,
    home: Option<String>,
    cache: Option<String>,
}

impl RuntimeInfo {
    fn collect() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            features: Features {
                typescript: true,
                type_check: false, // tsgo integration pending
                esm: true,
                vm: true, // Using otter-vm-core
            },
            paths: Paths {
                cwd: std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string()),
                home: dirs::home_dir().map(|p| p.display().to_string()),
                cache: dirs::cache_dir().map(|p| p.join("otter").display().to_string()),
            },
        }
    }
}
