# Markupsafe Performance Analysis — USE Method + Trace Instrumentation

Date: 2026-03-21

## Summary

Instrumented the orchestrator with Chrome Trace Format events and analyzed
worker utilization on the markupsafe corpus (479 mutants, 10 workers).

Key finding: **actual mutation work finishes in ~7.75s**, but wall clock is
~30s due to two pathological costs:

1. **Per-mutant timeout on dead workers** (~17s) — 2 workers die; the
   orchestrator waits 20s per timeout before recording them as errors.
2. **Sequential process shutdown** (~5s, now fixed to concurrent) — waiting
   for recycled worker processes to exit.

## Methodology

### Instrumentation

Added `src/trace.rs` — records Chrome Trace Format events during the worker
pool run. Events are written to `.irradiate/trace.json`, loadable in
[ui.perfetto.dev](https://ui.perfetto.dev).

Two event categories:
- `lifecycle` — worker startup spans (spawn → ready)
- `mutant` — per-mutant execution spans (dispatch → result)

### USE Method (Utilization, Saturation, Errors)

Applied to the worker pool as a queued system with N=10 workers.

## Results

### Trace-level timeline

```
Phase                    Duration    Notes
─────────────────────────────────────────────────────
Worker spawn (10×)       0ms         parallel, before trace epoch
Worker startup           188–253ms   pytest import + collection
First mutant dispatched  188ms       to first-ready worker
Last mutant result       7,750ms     477 mutant results collected
Error timeout wait       ~17,000ms   2 dead workers hit 20s timeout
Shutdown (concurrent)    5,000ms     kill_on_drop after timeout
─────────────────────────────────────────────────────
Total wall clock         ~30s
Actual useful work       ~7.75s (26% of wall clock)
```

### Utilization (U)

| Metric | Value |
|---|---|
| Workers | 10 concurrent, 29 total (recycled at 20 mutants) |
| Aggregate busy time | 66.9s |
| Theoretical min (10 workers) | 6.7s |
| Trace span (first dispatch → last result) | 7.75s |
| Worker utilization (within active lifetime) | **100%** |
| IPC gap (between mutants on same worker) | **0.005ms mean** |

Workers are never idle between mutants. The orchestrator dispatches the next
mutant in <10 microseconds after receiving a result. IPC is not a bottleneck.

### Saturation (S)

The work queue drains faster than workers consume it during the first ~4s
(all 10 workers busy, 200 mutants dispatched). After recycling, the second
wave of workers handles the remaining 279 mutants in ~3s.

No backpressure observed — the channel depth never exceeds 1 message.

### Errors (E)

2 of 479 mutants consistently error on markupsafe. These are workers that
crash or hang, detected only when the per-mutant socket read timeout fires.

**This is the dominant wall-clock cost.** The timeout floor is 20s
(`multiplier=10 × MIN_ESTIMATED_SECS=2.0`), so each error adds up to 20s
of blocking.

### Worker startup

| Workers | Mean startup | Max startup |
|---|---|---|
| Initial batch (W0–W9) | 220ms | 253ms |
| Recycled workers (W10–W28) | 123ms | 160ms |

Recycled workers start faster because Python's import cache is warm.

Total startup overhead across all 29 worker lifetimes: 3.65s. This is
amortized — workers start while others are still running.

### Worker recycling

Recycling fires every 20 mutants (auto-tuned due to session-scoped fixtures
detected in markupsafe's conftest.py). 479 mutants / 20 = 24 recycling
events, producing 29 total worker lifetimes.

Recycling cost per event: ~130ms (spawn + connect + ready). Total recycling
overhead: ~2.5s, fully overlapped with other workers' execution.

## py-spy

py-spy requires root on macOS. Run manually:

```bash
sudo py-spy record --subprocesses --format speedscope \
  -o /tmp/irradiate-markupsafe.speedscope.json \
  -- irradiate run --paths-to-mutate src/markupsafe --tests-dir tests --python .venv/bin/python3
```

Open the resulting file at [speedscope.app](https://speedscope.app).

## Optimization Opportunities (ordered by expected impact)

### 1. Fast worker death detection (~17s savings)

**Impact: eliminates the dominant wall-clock cost.**

Currently, dead workers are detected only when the per-mutant socket read
times out (20s). Options:

- **Process health check**: poll `proc.try_wait()` periodically (every
  100ms) in the main select loop. If a worker process exited, immediately
  mark its active mutant as error and reassign.
- **Socket error detection**: the reader task in `spawn_worker_task` already
  detects EOF/errors on the socket — but the per-read timeout delays this.
  Reduce the timeout or add a parallel watchdog that checks process liveness.
- **Reduce MIN_ESTIMATED_SECS**: lowering from 2.0 to 0.5 would give a 5s
  floor instead of 20s. Helps but doesn't fix the root cause.

### 2. Fork-from-warm-parent model

**Impact: eliminates worker startup + recycling overhead, improves isolation.**

Instead of replaying pytest items inside a long-lived session:

1. Start one warm parent (imports everything, collects tests)
2. `os.fork()` per mutant from the warm parent
3. Child runs tests normally via `pytest.main()`
4. Child exits, parent reaps

Gives copy-on-write isolation for free, normal pytest fixture behavior,
and ~0 startup cost per mutant. This is what mutmut 3 does.

### 3. Reduce recycling frequency

Currently recycling every 20 mutants due to session fixtures. If worker
death detection is fast enough, the correctness benefit of frequent
recycling may not justify the ~130ms startup cost per recycle.

### 4. Mutant batching

Send N mutants to a worker in a single IPC message. Worker processes them
sequentially and returns results as a batch. Eliminates per-mutant IPC
round-trip (currently <10us, so this is low priority).

## Trace file

The trace file is at `.irradiate/trace.json` after any `irradiate run`.
Open in [ui.perfetto.dev](https://ui.perfetto.dev) to visualize the worker
timeline.

Each worker appears as a separate "thread" row. Mutant executions are spans
colored by worker. Gaps between spans represent idle time (none observed on
markupsafe — workers are 100% utilized).
