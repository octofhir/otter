//! Info command - show runtime and environment information.

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct InfoCommand {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

impl InfoCommand {
    pub fn run(&self) -> Result<()> {
        let info = RuntimeInfo::collect();

        if self.json {
            println!("{}", serde_json::to_string_pretty(&info)?);
        } else {
            self.print_human_readable(&info);
        }

        Ok(())
    }

    fn print_human_readable(&self, info: &RuntimeInfo) {
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
                "disabled"
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
            "  JSC Engine:  {}",
            if info.features.jsc {
                "enabled"
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
}

#[derive(serde::Serialize)]
struct RuntimeInfo {
    version: String,
    platform: String,
    arch: String,
    features: Features,
    paths: Paths,
}

#[derive(serde::Serialize)]
struct Features {
    typescript: bool,
    type_check: bool,
    esm: bool,
    jsc: bool,
}

#[derive(serde::Serialize)]
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
                type_check: true,
                esm: true,
                jsc: true,
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
