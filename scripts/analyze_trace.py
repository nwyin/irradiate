#!/usr/bin/env python3
"""Analyze an irradiate trace.json to find performance bottlenecks.

Usage:
    python scripts/analyze_trace.py [path/to/.irradiate/trace.json]

Produces:
    - Pipeline phase breakdown (wall-clock time per phase)
    - Worker utilization (% of time each worker was busy)
    - Idle gap analysis (time between mutant completions and next dispatch)
    - Slowest mutants (top N by execution time)
    - Worker startup overhead
    - Throughput over time (mutants/sec in rolling windows)
"""

import json
import sys
from collections import defaultdict
from pathlib import Path


def load_trace(path: str) -> list[dict]:
    with open(path) as f:
        data = json.load(f)
    return data.get("traceEvents", data) if isinstance(data, dict) else data


def analyze(events: list[dict]):
    # Separate event types
    pipeline_phases = []
    worker_startups = []
    mutant_executions = []
    metadata = []

    for e in events:
        ph = e.get("ph", "")
        cat = e.get("cat", "")
        if ph == "M":
            metadata.append(e)
            continue
        if ph != "X":
            continue

        if cat == "pipeline":
            pipeline_phases.append(e)
        elif cat == "lifecycle":
            worker_startups.append(e)
        elif cat == "mutant":
            mutant_executions.append(e)

    # ---- Pipeline Phase Breakdown ----
    print("=" * 70)
    print("PIPELINE PHASE BREAKDOWN")
    print("=" * 70)
    total_pipeline_us = 0
    phases = sorted(pipeline_phases, key=lambda e: e["ts"])
    for p in phases:
        dur_ms = p["dur"] / 1000
        total_pipeline_us += p["dur"]
        args_str = ""
        if p.get("args"):
            args_str = "  " + ", ".join(f"{k}={v}" for k, v in p["args"].items())
        print(f"  {p['name']:<25} {dur_ms:>10.1f}ms{args_str}")
    print(f"  {'TOTAL':<25} {total_pipeline_us / 1000:>10.1f}ms")
    print()

    if not mutant_executions:
        print("No mutant execution events found.")
        return

    # ---- Worker Startup Overhead ----
    print("=" * 70)
    print("WORKER STARTUP OVERHEAD")
    print("=" * 70)
    if worker_startups:
        startup_durations = [e["dur"] / 1000 for e in worker_startups]
        print(f"  Workers spawned:   {len(startup_durations)}")
        print(f"  Mean startup:      {sum(startup_durations) / len(startup_durations):.1f}ms")
        print(f"  Max startup:       {max(startup_durations):.1f}ms")
        print(f"  Min startup:       {min(startup_durations):.1f}ms")
        print(f"  Total overhead:    {sum(startup_durations):.1f}ms")
    else:
        print("  No startup events found.")
    print()

    # ---- Worker Utilization ----
    print("=" * 70)
    print("WORKER UTILIZATION")
    print("=" * 70)

    # Find the worker pool phase boundaries
    pool_phase = next((p for p in phases if p["name"] == "worker_pool"), None)
    if pool_phase:
        pool_start = pool_phase["ts"]
        pool_end = pool_phase["ts"] + pool_phase["dur"]
        pool_dur_ms = pool_phase["dur"] / 1000
    else:
        # Fall back to mutant execution span
        pool_start = min(e["ts"] for e in mutant_executions)
        pool_end = max(e["ts"] + e["dur"] for e in mutant_executions)
        pool_dur_ms = (pool_end - pool_start) / 1000

    # Per-worker busy time
    worker_busy = defaultdict(int)  # tid -> total busy us
    worker_mutant_count = defaultdict(int)
    for e in mutant_executions:
        worker_busy[e["tid"]] += e["dur"]
        worker_mutant_count[e["tid"]] += 1

    worker_ids = sorted(worker_busy.keys())
    total_busy = 0
    total_capacity = 0
    print(f"  Pool wall-clock: {pool_dur_ms:.1f}ms")
    print()
    print(f"  {'Worker':<12} {'Busy(ms)':<12} {'Mutants':<10} {'Util%':<10} {'Avg(ms)':<10}")
    print(f"  {'-' * 54}")
    for wid in worker_ids:
        busy_ms = worker_busy[wid] / 1000
        count = worker_mutant_count[wid]
        util = (worker_busy[wid] / (pool_end - pool_start)) * 100 if pool_end > pool_start else 0
        avg_ms = busy_ms / count if count > 0 else 0
        total_busy += worker_busy[wid]
        total_capacity += (pool_end - pool_start)
        label = metadata_name(metadata, wid) or f"tid {wid}"
        print(f"  {label:<12} {busy_ms:>10.1f}  {count:>8}  {util:>8.1f}%  {avg_ms:>8.1f}")
    overall_util = (total_busy / total_capacity * 100) if total_capacity > 0 else 0
    print(f"\n  Overall utilization: {overall_util:.1f}%")
    print()

    # ---- Idle Gap Analysis ----
    print("=" * 70)
    print("IDLE GAP ANALYSIS (per worker)")
    print("=" * 70)
    # For each worker, sort mutant events by start time and measure gaps
    worker_events = defaultdict(list)
    for e in mutant_executions:
        worker_events[e["tid"]].append(e)

    total_idle_us = 0
    for wid in worker_ids:
        evts = sorted(worker_events[wid], key=lambda e: e["ts"])
        gaps = []
        for i in range(1, len(evts)):
            gap = evts[i]["ts"] - (evts[i - 1]["ts"] + evts[i - 1]["dur"])
            if gap > 0:
                gaps.append(gap)
        if gaps:
            total_gap_ms = sum(gaps) / 1000
            total_idle_us += sum(gaps)
            avg_gap_ms = total_gap_ms / len(gaps)
            max_gap_ms = max(gaps) / 1000
            label = metadata_name(metadata, wid) or f"tid {wid}"
            print(f"  {label}: {len(gaps)} gaps, total={total_gap_ms:.1f}ms, avg={avg_gap_ms:.1f}ms, max={max_gap_ms:.1f}ms")

    if total_idle_us > 0:
        print(f"\n  Total idle time across workers: {total_idle_us / 1000:.1f}ms")
        print(f"  Idle as % of capacity: {total_idle_us / total_capacity * 100:.1f}%")
    print()

    # ---- Slowest Mutants ----
    print("=" * 70)
    print("SLOWEST MUTANTS (top 20)")
    print("=" * 70)
    sorted_mutants = sorted(mutant_executions, key=lambda e: e["dur"], reverse=True)
    for i, e in enumerate(sorted_mutants[:20]):
        dur_ms = e["dur"] / 1000
        status = e.get("args", {}).get("status", "?")
        label = metadata_name(metadata, e["tid"]) or f"tid {e['tid']}"
        print(f"  {i + 1:>3}. {e['name']:<55} {dur_ms:>8.1f}ms  {status:<10} ({label})")
    print()

    # ---- Fastest Mutants (sanity check) ----
    print("=" * 70)
    print("FASTEST MUTANTS (bottom 10)")
    print("=" * 70)
    for i, e in enumerate(sorted_mutants[-10:]):
        dur_ms = e["dur"] / 1000
        status = e.get("args", {}).get("status", "?")
        print(f"  {i + 1:>3}. {e['name']:<55} {dur_ms:>8.1f}ms  {status}")
    print()

    # ---- Duration Distribution ----
    print("=" * 70)
    print("DURATION DISTRIBUTION")
    print("=" * 70)
    durations_ms = sorted(e["dur"] / 1000 for e in mutant_executions)
    n = len(durations_ms)
    if n > 0:
        p50 = durations_ms[n // 2]
        p90 = durations_ms[int(n * 0.9)]
        p95 = durations_ms[int(n * 0.95)]
        p99 = durations_ms[int(n * 0.99)]
        mean = sum(durations_ms) / n
        print(f"  Count:  {n}")
        print(f"  Mean:   {mean:.1f}ms")
        print(f"  P50:    {p50:.1f}ms")
        print(f"  P90:    {p90:.1f}ms")
        print(f"  P95:    {p95:.1f}ms")
        print(f"  P99:    {p99:.1f}ms")
        print(f"  Min:    {durations_ms[0]:.1f}ms")
        print(f"  Max:    {durations_ms[-1]:.1f}ms")

        # Histogram
        print("\n  Duration histogram:")
        buckets = [10, 50, 100, 200, 500, 1000, 2000, 5000, 10000, float("inf")]
        bucket_labels = ["<10ms", "<50ms", "<100ms", "<200ms", "<500ms", "<1s", "<2s", "<5s", "<10s", "10s+"]
        counts = [0] * len(buckets)
        for d in durations_ms:
            for j, b in enumerate(buckets):
                if d < b:
                    counts[j] += 1
                    break
        max_count = max(counts) if counts else 1
        for label, count in zip(bucket_labels, counts):
            bar = "#" * int(count / max_count * 40) if max_count > 0 else ""
            print(f"    {label:>8} | {bar:<40} {count:>5} ({count / n * 100:>5.1f}%)")
    print()

    # ---- Throughput Over Time ----
    print("=" * 70)
    print("THROUGHPUT (completions per 5-second window)")
    print("=" * 70)
    if mutant_executions:
        completions = sorted(e["ts"] + e["dur"] for e in mutant_executions)
        window_us = 5_000_000  # 5 seconds
        t = completions[0]
        end = completions[-1]
        while t < end:
            count = sum(1 for c in completions if t <= c < t + window_us)
            t_sec = (t - completions[0]) / 1_000_000
            rate = count / (window_us / 1_000_000)
            bar = "#" * int(rate)
            print(f"  {t_sec:>6.0f}s | {bar:<50} {rate:.1f}/s ({count})")
            t += window_us
    print()

    # ---- Summary ----
    print("=" * 70)
    print("SUMMARY")
    print("=" * 70)
    print(f"  Total mutants executed: {len(mutant_executions)}")
    print(f"  Total wall-clock:      {total_pipeline_us / 1_000_000:.1f}s")
    if pool_dur_ms > 0:
        print(f"  Worker pool time:      {pool_dur_ms / 1000:.1f}s")
        non_pool = total_pipeline_us / 1000 - pool_dur_ms
        print(f"  Non-pool overhead:     {non_pool:.1f}ms ({non_pool / (total_pipeline_us / 1000) * 100:.1f}%)")
    print(f"  Overall utilization:   {overall_util:.1f}%")
    print(f"  Effective throughput:  {len(mutant_executions) / (total_pipeline_us / 1_000_000):.1f} mutants/s")
    print()


def metadata_name(metadata: list[dict], tid: int) -> str | None:
    for m in metadata:
        if m.get("tid") == tid and m.get("args", {}).get("name"):
            return m["args"]["name"]
    return None


def main():
    if len(sys.argv) > 1:
        path = sys.argv[1]
    else:
        # Try default location
        candidates = [
            Path.cwd() / ".irradiate" / "trace.json",
            Path.home() / "projects" / "hive" / ".irradiate" / "trace.json",
        ]
        path = next((str(p) for p in candidates if p.exists()), None)
        if not path:
            print(f"Usage: {sys.argv[0]} <trace.json>", file=sys.stderr)
            sys.exit(1)

    print(f"Analyzing: {path}\n")
    events = load_trace(path)
    analyze(events)


if __name__ == "__main__":
    main()
