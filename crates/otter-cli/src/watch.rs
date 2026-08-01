//! Restart-on-change runs.
//!
//! `otter watch <target>` runs a target and runs it again whenever a project
//! source file changes. The run happens in a child process rather than in this
//! one: a program under development leaves state behind — open handles,
//! registered globals, a half-built module graph — and the only restart that
//! is reliably clean is a fresh process.
//!
//! # Contents
//! - [`WatchArgs`] — the command's arguments.
//! - [`run_watch`] — the watch loop.
//!
//! # Invariants
//! - Exactly one child runs at a time. A change during a run replaces that
//!   run; it never starts a second one alongside it.
//! - Changes are debounced, because an editor's save is several filesystem
//!   events and a restart per event is a restart storm.
//! - Directories that no edit of the project's own sources passes through —
//!   `node_modules`, version-control and cache directories — are ignored, so a
//!   dependency install does not look like an edit.
//! - Interrupting the watcher terminates the child; a run never outlives the
//!   watcher that started it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::mpsc;
use std::time::Duration;

use clap::Args;
use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;

use crate::OtterError;

/// How long a burst of filesystem events is allowed to settle before a
/// restart. Long enough to absorb one editor save, short enough to feel
/// immediate.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Directory names a project edit never passes through.
const IGNORED_DIRECTORIES: &[&str] = &["node_modules", ".git", ".otter", "target", "dist"];

/// Arguments for a restart-on-change run.
#[derive(Debug, Args)]
pub(crate) struct WatchArgs {
    /// File path, package script, or local package binary.
    pub(crate) target: String,
    /// Directory to watch. Defaults to the working directory.
    #[arg(long)]
    pub(crate) dir: Option<PathBuf>,
    /// Forwarded target arguments.
    #[arg(trailing_var_arg = true)]
    pub(crate) args: Vec<String>,
}

/// Run `args.target`, restarting it whenever a watched source file changes.
pub(crate) fn run_watch(args: WatchArgs) -> Result<ExitCode, OtterError> {
    let root = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|err| crate::pm_config_error(err.to_string()))?,
    };
    let executable =
        std::env::current_exe().map_err(|err| crate::pm_config_error(err.to_string()))?;

    let (sender, receiver) = mpsc::channel();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result| {
        // A watcher error is not worth stopping a development loop over; the
        // next event still arrives, and a missed one costs one manual restart.
        if let Ok(events) = result {
            let _ = sender.send(events);
        }
    })
    .map_err(|err| crate::pm_config_error(err.to_string()))?;
    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|err| crate::pm_config_error(err.to_string()))?;

    let mut child = spawn_target(&executable, &args)?;
    while let Ok(events) = receiver.recv() {
        let changed = events
            .iter()
            .flat_map(|event| event.paths.iter())
            .any(|path| is_watched_path(&root, path));
        if !changed {
            continue;
        }
        let _ = writeln!(std::io::stderr(), "restarting: file change detected");
        terminate(&mut child);
        child = spawn_target(&executable, &args)?;
    }
    terminate(&mut child);
    Ok(ExitCode::SUCCESS)
}

fn spawn_target(executable: &Path, args: &WatchArgs) -> Result<Child, OtterError> {
    Command::new(executable)
        .arg("run")
        .arg(&args.target)
        .args(&args.args)
        .spawn()
        .map_err(|err| crate::pm_config_error(err.to_string()))
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `true` when a changed path is one an edit of the project's own sources
/// would produce.
fn is_watched_path(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            continue;
        };
        let part = part.to_string_lossy();
        if IGNORED_DIRECTORIES.contains(&part.as_ref()) {
            return false;
        }
    }
    // A directory event carries no extension and says nothing on its own; a
    // file inside it produces its own event.
    path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().as_ref(),
            "ts" | "mts"
                | "cts"
                | "tsx"
                | "js"
                | "mjs"
                | "cjs"
                | "jsx"
                | "json"
                | "jsonc"
                | "json5"
                | "yaml"
                | "yml"
                | "toml"
                | "txt"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_sources_are_watched() {
        let root = Path::new("/project");
        assert!(is_watched_path(root, Path::new("/project/src/index.ts")));
        assert!(is_watched_path(root, Path::new("/project/config.yaml")));
        assert!(is_watched_path(root, Path::new("/project/package.json")));
    }

    #[test]
    fn installed_and_generated_directories_are_not_watched() {
        let root = Path::new("/project");
        assert!(!is_watched_path(
            root,
            Path::new("/project/node_modules/left-pad/index.js")
        ));
        assert!(!is_watched_path(root, Path::new("/project/.git/HEAD")));
        assert!(!is_watched_path(root, Path::new("/project/dist/bundle.js")));
    }

    #[test]
    fn files_that_are_not_sources_are_not_watched() {
        let root = Path::new("/project");
        assert!(!is_watched_path(root, Path::new("/project/README.md")));
        assert!(!is_watched_path(root, Path::new("/project/src")));
        assert!(!is_watched_path(root, Path::new("/project/image.png")));
    }
}
