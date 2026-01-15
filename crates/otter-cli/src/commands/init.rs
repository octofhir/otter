//! Init command - initialize a new Otter project.

use anyhow::Result;
use clap::Args;
use std::fs;
use std::path::PathBuf;

#[derive(Args)]
pub struct InitCommand {
    /// Project directory (defaults to current directory)
    pub path: Option<PathBuf>,

    /// Project name (defaults to directory name)
    #[arg(long)]
    pub name: Option<String>,

    /// Use TypeScript (default)
    #[arg(long, default_value_t = true)]
    pub typescript: bool,

    /// Skip creating example files
    #[arg(long)]
    pub bare: bool,
}

impl InitCommand {
    pub async fn run(&self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let project_dir = self.path.clone().unwrap_or(cwd.clone());

        // Create directory if it doesn't exist
        if !project_dir.exists() {
            fs::create_dir_all(&project_dir)?;
        }

        let project_name = self
            .name
            .clone()
            .or_else(|| {
                project_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "my-project".to_string());

        // Check if already initialized
        let package_json_path = project_dir.join("package.json");
        if package_json_path.exists() {
            anyhow::bail!("Project already initialized (package.json exists)");
        }

        println!("Creating new Otter project: {}", project_name);

        // Create package.json
        let package_json = serde_json::json!({
            "name": project_name,
            "version": "0.1.0",
            "type": "module",
            "main": "src/index.ts",
            "scripts": {
                "start": "otter run src/index.ts",
                "dev": "otter run --watch src/index.ts",
                "test": "otter test",
                "check": "otter check src/**/*.ts"
            },
            "dependencies": {},
            "devDependencies": {}
        });

        fs::write(
            project_dir.join("package.json"),
            serde_json::to_string_pretty(&package_json)?,
        )?;
        println!("  Created package.json");

        // Create otter.toml
        let otter_toml = r#"# Otter configuration

[typescript]
check = true
strict = true

[modules]
# Allowed remote module sources
remote_allowlist = [
    "https://esm.sh/*",
    "https://cdn.skypack.dev/*",
    "https://unpkg.com/*",
]

# Module cache directory
# cache_dir = ".otter/cache"

# Import map aliases
# [modules.import_map]
# "@/" = "./src/"

[permissions]
# Default permissions (can be overridden with CLI flags)
# allow_read = ["."]
# allow_write = []
# allow_net = []
# allow_env = []
"#;
        fs::write(project_dir.join("otter.toml"), otter_toml)?;
        println!("  Created otter.toml");

        // Create tsconfig.json
        let tsconfig = serde_json::json!({
            "compilerOptions": {
                "target": "ES2022",
                "module": "ESNext",
                "moduleResolution": "bundler",
                "strict": true,
                "esModuleInterop": true,
                "skipLibCheck": true,
                "forceConsistentCasingInFileNames": true,
                "noEmit": true,
                "allowImportingTsExtensions": true,
                "resolveJsonModule": true,
                "isolatedModules": true,
                "lib": ["ES2022"]
            },
            "include": ["src/**/*"],
            "exclude": ["node_modules"]
        });

        fs::write(
            project_dir.join("tsconfig.json"),
            serde_json::to_string_pretty(&tsconfig)?,
        )?;
        println!("  Created tsconfig.json");

        // Create .gitignore
        let gitignore = r#"# Dependencies
node_modules/

# Otter cache
.otter/

# Build output
dist/

# Environment files
.env
.env.local
.env.*.local

# IDE
.idea/
.vscode/
*.swp
*.swo

# OS
.DS_Store
Thumbs.db
"#;
        fs::write(project_dir.join(".gitignore"), gitignore)?;
        println!("  Created .gitignore");

        // Create src directory and example files
        if !self.bare {
            let src_dir = project_dir.join("src");
            fs::create_dir_all(&src_dir)?;

            // Create src/index.ts
            let index_ts = r#"// Welcome to Otter!

interface Greeting {
  message: string;
  timestamp: Date;
}

function greet(name: string): Greeting {
  return {
    message: `Hello, ${name}! Welcome to Otter.`,
    timestamp: new Date(),
  };
}

const greeting = greet("World");
console.log(greeting.message);
console.log(`Started at: ${greeting.timestamp.toISOString()}`);
"#;
            fs::write(src_dir.join("index.ts"), index_ts)?;
            println!("  Created src/index.ts");

            // Create example test file
            let test_dir = project_dir.join("src");
            let test_ts = r#"// Example test file

describe("greet", () => {
  it("should return a greeting message", () => {
    const result = "Hello, World!";
    expect(result).toContain("Hello");
  });

  it("should work with different names", () => {
    const name = "Otter";
    expect(name.length).toBeGreaterThan(0);
  });
});
"#;
            fs::write(test_dir.join("index.test.ts"), test_ts)?;
            println!("  Created src/index.test.ts");
        }

        println!();
        println!("Project created successfully!");
        println!();
        println!("Next steps:");

        if self.path.is_some() {
            println!("  cd {}", project_dir.display());
        }

        println!("  otter run src/index.ts    # Run the project");
        println!("  otter test                # Run tests");
        println!("  otter check src/**/*.ts   # Type check");

        Ok(())
    }
}
