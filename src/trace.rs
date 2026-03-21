//! Chrome Trace Format (CTF) event recording for worker pool visualization.
//!
//! Produces a JSON file loadable by <https://ui.perfetto.dev> or `chrome://tracing`.
//! Each worker appears as a separate "thread" (tid), with spans for startup and
//! per-mutant execution.

use serde::Serialize;
use std::path::Path;
use std::time::Instant;

/// A single Chrome Trace Format event.
#[derive(Debug, Serialize)]
pub struct TraceEvent {
    /// Event name (e.g., mutant name or "worker_startup").
    pub name: String,
    /// Category string (for filtering in the trace viewer).
    pub cat: &'static str,
    /// Phase: "X" = complete, "i" = instant.
    pub ph: &'static str,
    /// Timestamp in microseconds from trace start.
    pub ts: u64,
    /// Duration in microseconds (only for ph="X").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur: Option<u64>,
    /// Process ID (always 1 — the orchestrator).
    pub pid: u32,
    /// Thread ID — we use worker_id so each worker gets its own row.
    pub tid: usize,
    /// Freeform key-value metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// Collects trace events during a worker pool run.
pub struct TraceLog {
    epoch: Instant,
    pub events: Vec<TraceEvent>,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceLog {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            events: Vec::new(),
        }
    }

    /// Microseconds since trace start.
    pub fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Record a complete ("X") event with a known start time and duration.
    pub fn complete(
        &mut self,
        name: String,
        cat: &'static str,
        start_us: u64,
        dur_us: u64,
        tid: usize,
        args: Option<serde_json::Value>,
    ) {
        self.events.push(TraceEvent {
            name,
            cat,
            ph: "X",
            ts: start_us,
            dur: Some(dur_us),
            pid: 1,
            tid,
            args,
        });
    }

    /// Record an instant ("i") event.
    pub fn instant(
        &mut self,
        name: String,
        cat: &'static str,
        tid: usize,
        args: Option<serde_json::Value>,
    ) {
        self.events.push(TraceEvent {
            name,
            cat,
            ph: "i",
            ts: self.now_us(),
            dur: None,
            pid: 1,
            tid,
            args,
        });
    }
}

/// Write trace events to a JSON file in Chrome Trace Format.
pub fn write_trace_file(path: &Path, events: &[TraceEvent]) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct TraceFile<'a> {
        #[serde(rename = "traceEvents")]
        trace_events: &'a [TraceEvent],
    }

    let file = std::fs::File::create(path)?;
    serde_json::to_writer(file, &TraceFile { trace_events: events })?;
    Ok(())
}
