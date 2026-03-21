//! Live progress bar for mutation testing, shown only on TTY stderr.

use std::io::{IsTerminal, Write};
use std::time::Instant;

use crate::protocol::MutantStatus;

/// Tracks mutation testing progress and renders a live status line on TTY stderr.
/// No-ops when stderr is not a terminal (CI, piped output, etc.).
pub struct ProgressBar {
    total: usize,
    completed: usize,
    killed: usize,
    survived: usize,
    errors: usize,
    timeout: usize,
    start: Instant,
    is_tty: bool,
}

impl ProgressBar {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            completed: 0,
            killed: 0,
            survived: 0,
            errors: 0,
            timeout: 0,
            start: Instant::now(),
            is_tty: std::io::stderr().is_terminal(),
        }
    }

    /// Record a completed mutant and redraw the progress line.
    pub fn record(&mut self, status: MutantStatus) {
        self.completed += 1;
        match status {
            MutantStatus::Killed => self.killed += 1,
            MutantStatus::Survived => self.survived += 1,
            MutantStatus::Timeout => self.timeout += 1,
            MutantStatus::Error | MutantStatus::NoTests | MutantStatus::TypeCheck => {
                self.errors += 1
            }
        }
        self.render();
    }

    /// Render the progress bar to stderr (TTY only).
    fn render(&self) {
        if !self.is_tty {
            return;
        }

        let elapsed = self.start.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            self.completed as f64 / elapsed
        } else {
            0.0
        };

        let pct = if self.total > 0 {
            self.completed * 100 / self.total
        } else {
            0
        };

        // Build bar: 30 chars wide
        let bar_width = 30;
        let filled = if self.total > 0 {
            bar_width * self.completed / self.total
        } else {
            0
        };
        let empty = bar_width - filled;
        let bar: String = "=".repeat(filled) + &" ".repeat(empty);

        let eta = if rate > 0.0 && self.completed < self.total {
            let remaining = (self.total - self.completed) as f64 / rate;
            if remaining < 60.0 {
                format!("{:.0}s", remaining)
            } else {
                format!("{:.0}m{:.0}s", remaining / 60.0, remaining % 60.0)
            }
        } else {
            "-".to_string()
        };

        let _ = write!(
            std::io::stderr(),
            "\r  [{bar}] {}/{} ({pct}%) | killed: {} | survived: {} | {:.1}/s | eta: {eta}  ",
            self.completed, self.total, self.killed, self.survived, rate,
        );
        let _ = std::io::stderr().flush();
    }

    /// Clear the progress line and print the final newline.
    pub fn finish(&self) {
        if !self.is_tty {
            return;
        }
        // Clear the line
        let _ = write!(std::io::stderr(), "\r{}\r", " ".repeat(100));
        let _ = std::io::stderr().flush();
    }
}
