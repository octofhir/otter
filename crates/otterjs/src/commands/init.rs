//! Init command - initialize a new Otter project.

use anyhow::Result;
use std::fs;

/// Run the init command
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;

    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
        .unwrap_or_else(|| "my-project".to_string());

    // Check if already initialized
    let package_json_path = cwd.join("package.json");
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
        cwd.join("package.json"),
        serde_json::to_string_pretty(&package_json)?,
    )?;
    println!("  Created package.json");

    // Create otter.toml
    let otter_toml = r#"# Otter configuration

[typescript]
check = false
strict = true

[modules]
# Allowed remote module sources
remote_allowlist = [
    "https://esm.sh/*",
    "https://cdn.skypack.dev/*",
    "https://unpkg.com/*",
]

[permissions]
# Default permissions (can be overridden with CLI flags)
# allow_read = ["."]
# allow_write = []
# allow_net = []
# allow_env = []
"#;
    fs::write(cwd.join("otter.toml"), otter_toml)?;
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
        cwd.join("tsconfig.json"),
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
    fs::write(cwd.join(".gitignore"), gitignore)?;
    println!("  Created .gitignore");

    // Create src directory and example files
    let src_dir = cwd.join("src");
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
    fs::write(src_dir.join("index.test.ts"), test_ts)?;
    println!("  Created src/index.test.ts");

    println!();
    println!("Project created successfully!");
    println!();
    println!("Next steps:");
    println!("  otter run src/index.ts    # Run the project");
    println!("  otter test                # Run tests");
    println!("  otter check src/**/*.ts   # Type check");

    Ok(())
}
