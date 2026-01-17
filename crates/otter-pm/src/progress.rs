//! Progress bar for package installation

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::time::Instant;

/// Progress tracker for package installation
pub struct InstallProgress {
    multi: MultiProgress,
    resolve_bar: Option<ProgressBar>,
    download_bar: Option<ProgressBar>,
    install_bar: Option<ProgressBar>,
    start_time: Instant,
    silent: bool,
}

impl InstallProgress {
    /// Create a new progress tracker
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            resolve_bar: None,
            download_bar: None,
            install_bar: None,
            start_time: Instant::now(),
            silent: false,
        }
    }

    /// Create a silent progress tracker (no output)
    pub fn silent() -> Self {
        Self {
            multi: MultiProgress::new(),
            resolve_bar: None,
            download_bar: None,
            install_bar: None,
            start_time: Instant::now(),
            silent: true,
        }
    }

    /// Start the resolve phase
    pub fn start_resolve(&mut self, count: usize) {
        if self.silent {
            return;
        }

        let spinner_style = ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {prefix} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style);
        bar.set_prefix(format!("{}", style("Resolving").cyan().bold()));
        bar.set_message(format!("{} dependencies...", count));
        bar.enable_steady_tick(std::time::Duration::from_millis(80));

        self.resolve_bar = Some(bar);
    }

    /// Finish the resolve phase
    pub fn finish_resolve(&mut self, count: usize) {
        if let Some(bar) = self.resolve_bar.take() {
            bar.finish_with_message(format!(
                "{} Resolved {} packages",
                style("✓").green().bold(),
                count
            ));
        }
    }

    /// Start the download phase
    pub fn start_download(&mut self, total: usize) {
        if self.silent {
            return;
        }

        let bar_style = ProgressStyle::default_bar()
            .template("{prefix} [{bar:40.cyan/dim}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("━━╸");

        let bar = self.multi.add(ProgressBar::new(total as u64));
        bar.set_style(bar_style);
        bar.set_prefix(format!("{}", style("Downloading").cyan().bold()));
        bar.set_message("");

        self.download_bar = Some(bar);
    }

    /// Update download progress
    pub fn tick_download(&mut self, name: &str) {
        if let Some(bar) = &self.download_bar {
            bar.inc(1);
            bar.set_message(format!("{}", style(name).dim()));
        }
    }

    /// Finish download phase
    pub fn finish_download(&mut self) {
        if let Some(bar) = self.download_bar.take() {
            let count = bar.position();
            bar.finish_with_message(format!(
                "{} Downloaded {} packages",
                style("✓").green().bold(),
                count
            ));
        }
    }

    /// Start the install/extract phase
    pub fn start_install(&mut self, total: usize) {
        if self.silent {
            return;
        }

        let bar_style = ProgressStyle::default_bar()
            .template("{prefix} [{bar:40.cyan/dim}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("━━╸");

        let bar = self.multi.add(ProgressBar::new(total as u64));
        bar.set_style(bar_style);
        bar.set_prefix(format!("{}", style("Installing").cyan().bold()));
        bar.set_message("");

        self.install_bar = Some(bar);
    }

    /// Update install progress
    pub fn tick_install(&mut self, name: &str) {
        if let Some(bar) = &self.install_bar {
            bar.inc(1);
            bar.set_message(format!("{}", style(name).dim()));
        }
    }

    /// Finish install phase
    pub fn finish_install(&mut self) {
        if let Some(bar) = self.install_bar.take() {
            let count = bar.position();
            bar.finish_with_message(format!(
                "{} Installed {} packages",
                style("✓").green().bold(),
                count
            ));
        }
    }

    /// Print final summary
    pub fn finish(&self, package_count: usize) {
        if self.silent {
            return;
        }

        let elapsed = self.start_time.elapsed();
        let secs = elapsed.as_secs_f64();

        println!(
            "\n{} Done in {:.1}s ({} packages)",
            style("✨").bold(),
            secs,
            package_count
        );
    }

    /// Print error message
    pub fn error(&self, msg: &str) {
        if self.silent {
            return;
        }
        eprintln!("{} {}", style("✗").red().bold(), msg);
    }

    /// Print warning message
    pub fn warn(&self, msg: &str) {
        if self.silent {
            return;
        }
        eprintln!("{} {}", style("⚠").yellow().bold(), msg);
    }
}

impl Default for InstallProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for InstallProgress {
    fn clone(&self) -> Self {
        // Progress bars are shared, so we create a silent clone for parallel tasks
        // The main progress tracker handles the actual display
        InstallProgress::silent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_silent() {
        let progress = InstallProgress::silent();
        assert!(progress.silent);
    }

    #[test]
    fn test_progress_lifecycle() {
        let mut progress = InstallProgress::silent();
        progress.start_resolve(5);
        progress.finish_resolve(5);
        progress.start_download(10);
        progress.tick_download("test-package");
        progress.finish_download();
        progress.finish(10);
    }
}
