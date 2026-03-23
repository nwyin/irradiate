//! Chrome Trace Format (CTF) event recording for full pipeline visualization.
//!
//! Produces a JSON file loadable by <https://ui.perfetto.dev> or `chrome://tracing`.
//! Pipeline phases appear on tid 0 ("pipeline"), each worker on its own tid (1+).

use serde::Serialize;
use std::path::Path;
use std::time::Instant;

/// Thread ID for pipeline-level phases (appears as row 0 in Perfetto).
pub const TID_PIPELINE: usize = 0;

/// Worker thread IDs start at this offset so they don't collide with pipeline.
pub const TID_WORKER_OFFSET: usize = 1;

/// A single Chrome Trace Format event.
#[derive(Debug, Clone, Serialize)]
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

    /// Record a pipeline-level phase span on tid 0.
    pub fn phase(&mut self, name: &str, start_us: u64, args: Option<serde_json::Value>) {
        let now = self.now_us();
        self.complete(
            name.to_string(),
            "pipeline",
            start_us,
            now.saturating_sub(start_us),
            TID_PIPELINE,
            args,
        );
    }

    /// Merge events from another TraceLog, adjusting timestamps relative to this log's epoch.
    pub fn merge(&mut self, other: TraceLog) {
        // Both logs use Instant epochs — compute the offset between them.
        let offset_us = if other.epoch >= self.epoch {
            other.epoch.duration_since(self.epoch).as_micros() as u64
        } else {
            0 // other started before us (shouldn't happen, but be safe)
        };
        for mut event in other.events {
            event.ts += offset_us;
            // Shift worker tids to avoid collision with pipeline tid
            if event.cat != "pipeline" {
                event.tid += TID_WORKER_OFFSET;
            }
            self.events.push(event);
        }
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
///
/// Prepends thread-name metadata events so Perfetto labels rows nicely.
pub fn write_trace_file(path: &Path, events: &[TraceEvent]) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct TraceFile<'a> {
        #[serde(rename = "traceEvents")]
        trace_events: &'a [TraceEvent],
    }

    // Collect unique tids and generate thread-name metadata events.
    let mut tids: Vec<usize> = events.iter().map(|e| e.tid).collect();
    tids.sort_unstable();
    tids.dedup();

    let mut all_events: Vec<TraceEvent> = tids
        .iter()
        .map(|&tid| {
            let label = if tid == TID_PIPELINE {
                "pipeline".to_string()
            } else {
                format!("worker {}", tid - TID_WORKER_OFFSET)
            };
            TraceEvent {
                name: "thread_name".to_string(),
                cat: "",
                ph: "M",
                ts: 0,
                dur: None,
                pid: 1,
                tid,
                args: Some(serde_json::json!({ "name": label })),
            }
        })
        .collect();
    all_events.extend_from_slice(events);

    let file = std::fs::File::create(path)?;
    serde_json::to_writer(file, &TraceFile { trace_events: &all_events })?;
    Ok(())
}
