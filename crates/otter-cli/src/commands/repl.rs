//! REPL command - interactive TypeScript/JavaScript shell.

use anyhow::Result;
use clap::Args;
use otter_runtime::{JscConfig, JscRuntime, transpile_typescript};
use std::io::{self, BufRead, Write};

use crate::config::Config;

#[derive(Args)]
pub struct ReplCommand {
    /// Allow all permissions in REPL
    #[arg(long = "allow-all", short = 'A')]
    pub allow_all: bool,

    /// Evaluate code and exit
    #[arg(long, short = 'e')]
    pub eval: Option<String>,
}

impl ReplCommand {
    pub async fn run(&self, _config: &Config) -> Result<()> {
        // If --eval is provided, just run that and exit
        if let Some(ref code) = self.eval {
            return self.eval_and_exit(code);
        }

        println!("Otter {} - TypeScript Runtime", env!("CARGO_PKG_VERSION"));
        println!("Type .help for help, .exit to exit\n");

        let runtime = JscRuntime::new(JscConfig::default())?;

        let stdin = io::stdin();
        let mut stdout = io::stdout();

        let mut multiline_buffer = String::new();
        let mut in_multiline = false;

        loop {
            // Print prompt
            let prompt = if in_multiline { "...> " } else { "otter> " };
            print!("{}", prompt);
            stdout.flush()?;

            // Read line
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Error reading input: {}", e);
                    break;
                }
            }

            let line = line.trim_end();

            // Handle empty input
            if line.is_empty() && !in_multiline {
                continue;
            }

            // Handle REPL commands
            if line.starts_with('.') && !in_multiline {
                match line {
                    ".exit" | ".quit" | ".q" => break,
                    ".help" | ".h" => {
                        self.print_help();
                        continue;
                    }
                    ".clear" | ".cls" => {
                        print!("\x1B[2J\x1B[1;1H");
                        stdout.flush()?;
                        continue;
                    }
                    ".multiline" | ".m" => {
                        in_multiline = true;
                        println!(
                            "Entering multiline mode. Type .end to execute, .cancel to abort."
                        );
                        continue;
                    }
                    ".end" => {
                        if in_multiline {
                            let code = std::mem::take(&mut multiline_buffer);
                            in_multiline = false;
                            self.eval_line(&runtime, &code);
                        }
                        continue;
                    }
                    ".cancel" => {
                        multiline_buffer.clear();
                        in_multiline = false;
                        println!("Multiline input cancelled.");
                        continue;
                    }
                    _ => {
                        println!(
                            "Unknown command: {}. Type .help for available commands.",
                            line
                        );
                        continue;
                    }
                }
            }

            // Handle multiline mode
            if in_multiline {
                multiline_buffer.push_str(line);
                multiline_buffer.push('\n');
                continue;
            }

            // Evaluate single line
            self.eval_line(&runtime, line);
        }

        println!("\nGoodbye!");
        Ok(())
    }

    fn eval_and_exit(&self, code: &str) -> Result<()> {
        let runtime = JscRuntime::new(JscConfig::default())?;

        // Try to transpile as TypeScript
        let js_code = match transpile_typescript(code) {
            Ok(result) => result.code,
            Err(_) => code.to_string(), // If transpilation fails, try as JS
        };

        match runtime.eval(&js_code) {
            Ok(value) => {
                if !value.is_null() && !value.is_undefined() {
                    println!(
                        "{}",
                        value.to_json().unwrap_or_else(|_| "undefined".to_string())
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }

        Ok(())
    }

    fn eval_line(&self, runtime: &JscRuntime, line: &str) {
        // Try to transpile as TypeScript
        let js_code = match transpile_typescript(line) {
            Ok(result) => result.code,
            Err(_) => line.to_string(), // If transpilation fails, try as JS
        };

        match runtime.eval(&js_code) {
            Ok(value) => {
                if !value.is_null() && !value.is_undefined() {
                    // Try to pretty print
                    match value.to_json() {
                        Ok(json) => println!("{}", json),
                        Err(_) => println!("{:?}", value),
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {}", e);
            }
        }
    }

    fn print_help(&self) {
        println!("REPL Commands:");
        println!("  .help, .h      Show this help message");
        println!("  .exit, .q      Exit the REPL");
        println!("  .clear, .cls   Clear the screen");
        println!("  .multiline, .m Enter multiline mode");
        println!("  .end           Execute multiline input");
        println!("  .cancel        Cancel multiline input");
        println!();
        println!("You can type any JavaScript or TypeScript expression.");
        println!("TypeScript will be transpiled automatically.");
    }
}
