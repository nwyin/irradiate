//! Live progress display for mutation testing, shown only on TTY stderr.
//!
//! Renders a main progress bar plus per-worker activity lines, similar to
//! how uv shows parallel downloads.  Throttled to avoid excessive I/O.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::Instant;

use crate::protocol::MutantStatus;

/// Tracks mutation testing progress and renders a live multi-line display.
/// No-ops when stderr is not a terminal (CI, piped output, etc.).
pub struct ProgressBar {
    total: usize,
    completed: usize,
    killed: usize,
    survived: usize,
    errors: usize,
    timeout: usize,
    start: Instant,
    last_render: Instant,
    is_tty: bool,
    /// Currently active workers: worker_id → short mutant label.
    active_workers: HashMap<usize, String>,
    /// Number of lines we printed last render (for clearing).
    last_line_count: usize,
}

/// Minimum interval between renders (100ms).
const RENDER_INTERVAL_MS: u128 = 100;

impl ProgressBar {
    pub fn new(total: usize) -> Self {
        let now = Instant::now();
        Self {
            total,
            completed: 0,
            killed: 0,
            survived: 0,
            errors: 0,
            timeout: 0,
            start: now,
            last_render: now - std::time::Duration::from_millis(RENDER_INTERVAL_MS as u64),
            is_tty: std::io::stderr().is_terminal(),
            active_workers: HashMap::new(),
            last_line_count: 0,
        }
    }

    /// A worker started testing a mutant.
    pub fn worker_start(&mut self, worker_id: usize, mutant_name: &str) {
        let label = shorten_mutant_name(mutant_name);
        self.active_workers.insert(worker_id, label);
        self.maybe_render();
    }

    /// A worker finished testing a mutant.
    pub fn worker_done(&mut self, worker_id: usize) {
        self.active_workers.remove(&worker_id);
    }

    /// Record a completed mutant and redraw.
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
        self.maybe_render();
    }

    /// Render only if enough time has passed since last render, or if we're done.
    fn maybe_render(&mut self) {
        if !self.is_tty {
            return;
        }
        let now = Instant::now();
        let force = self.completed >= self.total;
        if !force && now.duration_since(self.last_render).as_millis() < RENDER_INTERVAL_MS {
            return;
        }
        self.last_render = now;
        self.render();
    }

    /// Render the multi-line progress display.
    fn render(&mut self) {
        if !self.is_tty {
            return;
        }

        let mut out = String::with_capacity(512);

        // Move cursor up to overwrite previous output.
        if self.last_line_count > 0 {
            for _ in 0..self.last_line_count {
                out.push_str("\x1b[A\x1b[2K");
            }
        }
        out.push('\r');

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

        // Main progress line
        let bar_str: String = "\x1b[32m".to_string()
            + &"━".repeat(filled)
            + "\x1b[0m"
            + &"\x1b[2m━\x1b[0m".repeat(bar_width - filled);
        out.push_str(&format!(
            "  [{bar_str}] {}/{} ({pct}%) | \x1b[32m{}\x1b[0m killed | \x1b[31m{}\x1b[0m survived | {:.1}/s | eta: {eta}",
            self.completed, self.total, self.killed, self.survived, rate,
        ));
        out.push('\n');

        let mut line_count = 1;

        // Worker activity lines (cap at 8 to avoid scrolling)
        if !self.active_workers.is_empty() {
            let mut workers: Vec<_> = self.active_workers.iter().collect();
            workers.sort_by_key(|(id, _)| *id);
            for (id, label) in workers.iter().take(8) {
                out.push_str(&format!("  \x1b[2m  w{}: {}\x1b[0m\n", id, label));
                line_count += 1;
            }
            if workers.len() > 8 {
                out.push_str(&format!(
                    "  \x1b[2m  ... and {} more workers\x1b[0m\n",
                    workers.len() - 8
                ));
                line_count += 1;
            }
        }

        self.last_line_count = line_count;

        let _ = write!(std::io::stderr(), "{out}");
        let _ = std::io::stderr().flush();
    }

    /// Clear the progress display and print the final newline.
    pub fn finish(&self) {
        if !self.is_tty {
            return;
        }
        let mut out = String::new();
        if self.last_line_count > 0 {
            for _ in 0..self.last_line_count {
                out.push_str("\x1b[A\x1b[2K");
            }
        }
        out.push('\r');
        let _ = write!(std::io::stderr(), "{out}");
        let _ = std::io::stderr().flush();
    }
}

/// Shorten a mutant name for display: "mypackage.submodule.x_func__irradiate_3" → "submodule::func #3"
fn shorten_mutant_name(name: &str) -> String {
    // Split off the irradiate suffix: "...x_func__irradiate_3" → ("...x_func", "3")
    let (base, num) = name
        .rsplit_once("__irradiate_")
        .unwrap_or((name, "?"));

    // Get the last dotted component: "mypackage.submodule.x_func" → "submodule.x_func"
    let short = if let Some(dot_pos) = base.rfind('.') {
        &base[dot_pos + 1..]
    } else {
        base
    };

    // Strip the "x_" prefix and handle class methods (xǁClassǁmethod → Class.method)
    let display = if let Some(class_method) = short.strip_prefix("xǁ") {
        // Class method: "xǁClassǁmethod" → "Class.method"
        class_method.replacen('ǁ', ".", 1)
    } else if let Some(stripped) = short.strip_prefix("x_") {
        stripped.to_string()
    } else {
        short.to_string()
    };

    format!("{display} #{num}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_top_level_function() {
        assert_eq!(
            shorten_mutant_name("simple_lib.x_add__irradiate_1"),
            "add #1"
        );
    }

    #[test]
    fn shorten_class_method() {
        assert_eq!(
            shorten_mutant_name("mylib.core.xǁMyClassǁprocess__irradiate_42"),
            "MyClass.process #42"
        );
    }

    #[test]
    fn shorten_nested_module() {
        assert_eq!(
            shorten_mutant_name("pkg.sub.deep.x_helper__irradiate_7"),
            "helper #7"
        );
    }
}
