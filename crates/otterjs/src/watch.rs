//! Hot Module Replacement (HMR) and file watching support.
//!
//! Provides efficient file watching with debouncing and module invalidation
//! for development workflows.

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebouncedEventKind, Debouncer, new_debouncer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Events emitted by the file watcher.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// Files have changed and need to be reloaded
    FilesChanged(Vec<PathBuf>),
    /// An error occurred while watching
    Error(String),
}

/// Module state that can be preserved across reloads.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct HmrState {
    /// Global state to preserve (JSON-serialized)
    pub preserved_state: Option<String>,
    /// List of modules that were loaded
    pub loaded_modules: Vec<String>,
}

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Debounce delay in milliseconds
    pub debounce_ms: u64,
    /// File extensions to watch
    pub extensions: Vec<String>,
    /// Directories to ignore
    pub ignore_dirs: Vec<String>,
    /// Whether to clear console on reload
    pub clear_console: bool,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 100,
            extensions: vec![
                "ts".to_string(),
                "tsx".to_string(),
                "js".to_string(),
                "jsx".to_string(),
                "json".to_string(),
            ],
            ignore_dirs: vec![
                "node_modules".to_string(),
                ".git".to_string(),
                "dist".to_string(),
                "build".to_string(),
                ".otter".to_string(),
            ],
            clear_console: true,
        }
    }
}

/// File watcher with HMR support.
pub struct FileWatcher {
    config: WatchConfig,
    watched_paths: Arc<Mutex<HashSet<PathBuf>>>,
    event_tx: Sender<WatchEvent>,
    event_rx: Receiver<WatchEvent>,
    #[allow(dead_code)]
    debouncer: Option<Debouncer<RecommendedWatcher>>,
}

impl FileWatcher {
    /// Create a new file watcher with the given configuration.
    pub fn new(config: WatchConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            config,
            watched_paths: Arc::new(Mutex::new(HashSet::new())),
            event_tx,
            event_rx,
            debouncer: None,
        }
    }

    /// Start watching a directory.
    pub fn watch(&mut self, path: &Path) -> Result<(), String> {
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let watched_paths = self.watched_paths.clone();

        // Add path to watched set
        {
            let mut paths = watched_paths.lock().unwrap();
            paths.insert(path.to_path_buf());
        }

        let mut debouncer = new_debouncer(
            Duration::from_millis(self.config.debounce_ms),
            move |result: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                match result {
                    Ok(events) => {
                        let changed_files: Vec<PathBuf> = events
                            .into_iter()
                            .filter(|e| e.kind == DebouncedEventKind::Any)
                            .map(|e| e.path)
                            .filter(|p| should_watch_file(p, &config))
                            .collect();

                        if !changed_files.is_empty() {
                            let _ = event_tx.send(WatchEvent::FilesChanged(changed_files));
                        }
                    }
                    Err(e) => {
                        let _ = event_tx.send(WatchEvent::Error(e.to_string()));
                    }
                }
            },
        )
        .map_err(|e| e.to_string())?;

        // Watch the directory
        debouncer
            .watcher()
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;

        self.debouncer = Some(debouncer);
        Ok(())
    }

    /// Add a file to watch.
    #[allow(dead_code)]
    pub fn add_file(&mut self, path: &Path) -> Result<(), String> {
        if let Some(ref mut debouncer) = self.debouncer {
            debouncer
                .watcher()
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(|e| e.to_string())?;

            let mut paths = self.watched_paths.lock().unwrap();
            paths.insert(path.to_path_buf());
        }
        Ok(())
    }

    /// Wait for the next file change event.
    pub fn wait_for_change(&self) -> Option<WatchEvent> {
        self.event_rx.recv().ok()
    }

    /// Try to get a pending event without blocking.
    #[allow(dead_code)]
    pub fn try_recv(&self) -> Option<WatchEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Get all watched paths.
    #[allow(dead_code)]
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.watched_paths.lock().unwrap().iter().cloned().collect()
    }
}

/// Check if a file should be watched based on configuration.
fn should_watch_file(path: &Path, config: &WatchConfig) -> bool {
    // Check if in ignored directory
    for component in path.components() {
        if let std::path::Component::Normal(name) = component
            && let Some(name_str) = name.to_str()
            && config.ignore_dirs.contains(&name_str.to_string())
        {
            return false;
        }
    }

    // Check extension
    if let Some(ext) = path.extension()
        && let Some(ext_str) = ext.to_str()
    {
        return config.extensions.contains(&ext_str.to_string());
    }

    false
}

/// Module dependency tracker for HMR.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ModuleTracker {
    /// Map of module URL to its dependencies
    dependencies: std::collections::HashMap<String, Vec<String>>,
    /// Map of file path to module URLs that depend on it
    dependents: std::collections::HashMap<PathBuf, Vec<String>>,
}

#[allow(dead_code)]
impl ModuleTracker {
    /// Create a new module tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a module and its dependencies.
    pub fn register(&mut self, module_url: &str, deps: Vec<String>) {
        self.dependencies.insert(module_url.to_string(), deps);
    }

    /// Register a file path for a module.
    pub fn register_file(&mut self, file_path: PathBuf, module_url: &str) {
        self.dependents
            .entry(file_path)
            .or_default()
            .push(module_url.to_string());
    }

    /// Get modules that need to be invalidated when a file changes.
    pub fn get_invalidated_modules(&self, changed_file: &Path) -> Vec<String> {
        let mut invalidated = Vec::new();
        let mut visited = HashSet::new();

        if let Some(modules) = self.dependents.get(changed_file) {
            for module in modules {
                self.collect_dependents(module, &mut invalidated, &mut visited);
            }
        }

        invalidated
    }

    /// Recursively collect all modules that depend on a given module.
    fn collect_dependents(
        &self,
        module: &str,
        invalidated: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        if visited.contains(module) {
            return;
        }
        visited.insert(module.to_string());
        invalidated.push(module.to_string());

        // Find modules that depend on this one
        for (url, deps) in &self.dependencies {
            if deps.contains(&module.to_string()) && !visited.contains(url) {
                self.collect_dependents(url, invalidated, visited);
            }
        }
    }

    /// Clear all tracked modules.
    pub fn clear(&mut self) {
        self.dependencies.clear();
        self.dependents.clear();
    }
}

/// Generate HMR runtime code for the JavaScript side.
pub fn hmr_runtime_code() -> &'static str {
    r#"
// Otter HMR Runtime
globalThis.__otter_hmr = globalThis.__otter_hmr || {
    // Map of module URL to its accept handlers
    acceptHandlers: new Map(),

    // Map of module URL to its dispose handlers
    disposeHandlers: new Map(),

    // Preserved module state
    moduleState: new Map(),

    // Accept hot updates for a module
    accept(moduleUrl, callback) {
        this.acceptHandlers.set(moduleUrl, callback);
    },

    // Register a dispose handler for cleanup
    dispose(moduleUrl, callback) {
        this.disposeHandlers.set(moduleUrl, callback);
    },

    // Preserve state across reloads
    preserveState(moduleUrl, state) {
        this.moduleState.set(moduleUrl, state);
    },

    // Get preserved state
    getPreservedState(moduleUrl) {
        const state = this.moduleState.get(moduleUrl);
        this.moduleState.delete(moduleUrl);
        return state;
    },

    // Called when modules are invalidated
    invalidate(moduleUrls) {
        for (const url of moduleUrls) {
            const dispose = this.disposeHandlers.get(url);
            if (dispose) {
                dispose();
            }
        }
    },

    // Called after modules are reloaded
    update(moduleUrls) {
        for (const url of moduleUrls) {
            const accept = this.acceptHandlers.get(url);
            if (accept) {
                const module = globalThis.__otter_modules?.[url];
                if (module) {
                    accept(module);
                }
            }
        }
    }
};

// Make HMR available via import.meta.hot
if (typeof import !== 'undefined') {
    Object.defineProperty(import, 'meta', {
        value: {
            hot: {
                accept(callback) {
                    // Current module URL would be injected
                    const moduleUrl = globalThis.__otter_current_module;
                    if (moduleUrl) {
                        globalThis.__otter_hmr.accept(moduleUrl, callback);
                    }
                },
                dispose(callback) {
                    const moduleUrl = globalThis.__otter_current_module;
                    if (moduleUrl) {
                        globalThis.__otter_hmr.dispose(moduleUrl, callback);
                    }
                },
                data: {},
                invalidate() {
                    // Force full reload
                    globalThis.__otter_hmr_full_reload = true;
                }
            }
        },
        configurable: true
    });
}
"#
}

/// Print a reload message to the console.
pub fn print_reload_message(files: &[PathBuf]) {
    let file_names: Vec<&str> = files
        .iter()
        .filter_map(|p| p.file_name())
        .filter_map(|n| n.to_str())
        .collect();

    if file_names.len() == 1 {
        println!("\x1b[36m[HMR]\x1b[0m File changed: {}", file_names[0]);
    } else {
        println!("\x1b[36m[HMR]\x1b[0m {} files changed", file_names.len());
    }
}

/// Clear the console if enabled.
pub fn clear_console(enabled: bool) {
    if enabled {
        print!("\x1b[2J\x1b[1;1H");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_config_default() {
        let config = WatchConfig::default();
        assert_eq!(config.debounce_ms, 100);
        assert!(config.extensions.contains(&"ts".to_string()));
        assert!(config.ignore_dirs.contains(&"node_modules".to_string()));
    }

    #[test]
    fn test_should_watch_file() {
        let config = WatchConfig::default();

        assert!(should_watch_file(Path::new("src/main.ts"), &config));
        assert!(should_watch_file(Path::new("lib/utils.js"), &config));
        assert!(!should_watch_file(
            Path::new("node_modules/lodash/index.js"),
            &config
        ));
        assert!(!should_watch_file(Path::new("src/main.css"), &config));
    }

    #[test]
    fn test_module_tracker() {
        let mut tracker = ModuleTracker::new();

        tracker.register("file:///app.ts", vec!["file:///utils.ts".to_string()]);
        tracker.register("file:///utils.ts", vec![]);
        tracker.register_file(PathBuf::from("/utils.ts"), "file:///utils.ts");

        let invalidated = tracker.get_invalidated_modules(Path::new("/utils.ts"));
        assert!(invalidated.contains(&"file:///utils.ts".to_string()));
    }

    #[test]
    fn test_hmr_runtime_code() {
        let code = hmr_runtime_code();
        assert!(code.contains("__otter_hmr"));
        assert!(code.contains("accept"));
        assert!(code.contains("dispose"));
    }
}
