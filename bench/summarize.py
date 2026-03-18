#!/usr/bin/env python3
"""
bench/summarize.py — Parse benchmark run results and produce a markdown table.

Parses /usr/bin/time -l output (macOS), irradiate stderr, and mutmut stdout/stderr.
Writes summary.md and raw_data.json to the result directory.

Usage:
    python bench/summarize.py <result_dir> --target <name> --ncpu N --runs N
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
from dataclasses import asdict, dataclass, field
from pathlib import Path


# ── Data structures ───────────────────────────────────────────────────────

@dataclass
class RunMetrics:
    wall_secs: float | None = None
    peak_rss_mb: float | None = None
    total_mutants: int | None = None
    killed: int | None = None
    survived: int | None = None
    mutants_per_sec: float | None = None


@dataclass
class ConfigSummary:
    config: str
    label: str
    runs: list[RunMetrics] = field(default_factory=list)

    def median_wall(self) -> float | None:
        vals = [r.wall_secs for r in self.runs if r.wall_secs is not None]
        return statistics.median(vals) if vals else None

    def min_wall(self) -> float | None:
        vals = [r.wall_secs for r in self.runs if r.wall_secs is not None]
        return min(vals) if vals else None

    def max_wall(self) -> float | None:
        vals = [r.wall_secs for r in self.runs if r.wall_secs is not None]
        return max(vals) if vals else None

    def median_rss(self) -> float | None:
        vals = [r.peak_rss_mb for r in self.runs if r.peak_rss_mb is not None]
        return statistics.median(vals) if vals else None

    def consensus_mutants(self) -> int | None:
        vals = [r.total_mutants for r in self.runs if r.total_mutants is not None]
        return vals[0] if vals else None

    def consensus_killed(self) -> int | None:
        vals = [r.killed for r in self.runs if r.killed is not None]
        return vals[0] if vals else None

    def consensus_survived(self) -> int | None:
        vals = [r.survived for r in self.runs if r.survived is not None]
        return vals[0] if vals else None

    def median_mps(self) -> float | None:
        vals = [r.mutants_per_sec for r in self.runs if r.mutants_per_sec is not None]
        return statistics.median(vals) if vals else None

    def mutation_score(self) -> float | None:
        k = self.consensus_killed()
        s = self.consensus_survived()
        if k is None or s is None:
            return None
        total = k + s
        return (k / total * 100) if total > 0 else None

    def ms_per_mutant(self) -> float | None:
        wall = self.median_wall()
        mutants = self.consensus_mutants()
        if wall is None or mutants is None or mutants == 0:
            return None
        return wall * 1000 / mutants


# ── Parsers ───────────────────────────────────────────────────────────────

def parse_time_file(path: Path) -> tuple[float | None, float | None]:
    """
    Parse macOS /usr/bin/time -l output.

    Relevant lines (approximate):
        "       4.21 real         0.82 user         0.21 sys"
        "      51380224  maximum resident set size"

    Returns (wall_secs, peak_rss_mb).
    """
    if not path.exists():
        return None, None

    text = path.read_text(errors="replace")
    wall = None
    rss_mb = None

    # Wall-clock time: "   N.NN real" (may also be "N real" without decimals)
    m = re.search(r"(\d+(?:\.\d+)?)\s+real", text)
    if m:
        wall = float(m.group(1))

    # Peak RSS in bytes: "   NNNNN  maximum resident set size"
    m = re.search(r"(\d+)\s+maximum resident set size", text)
    if m:
        rss_mb = int(m.group(1)) / (1024 * 1024)

    return wall, rss_mb


def parse_irradiate_stderr(path: Path) -> RunMetrics:
    """
    Parse irradiate stderr for mutant stats.

    Key line:
        "Mutation testing complete (87 mutants in 4.2s, 20 mutants/sec)"
    Followed by:
        "  Killed:    52"
        "  Survived:  23"
    """
    metrics = RunMetrics()
    if not path.exists():
        return metrics

    text = path.read_text(errors="replace")

    m = re.search(
        r"Mutation testing complete \((\d+) mutants in ([\d.]+)s,\s*([\d.]+) mutants/sec\)",
        text,
    )
    if m:
        metrics.total_mutants = int(m.group(1))
        metrics.mutants_per_sec = float(m.group(3))

    m = re.search(r"Killed:\s+(\d+)", text)
    if m:
        metrics.killed = int(m.group(1))

    m = re.search(r"Survived:\s+(\d+)", text)
    if m:
        metrics.survived = int(m.group(1))

    return metrics


def parse_mutmut_output(stdout_path: Path, stderr_path: Path) -> RunMetrics:
    """
    Parse mutmut stdout/stderr for mutant stats.

    mutmut stdout contains:
        "87/87  🎉 52 🫥 0  ⏰ 0  🤔 0  🙁 23  🔇 0  🧙 0"
        "4.00 mutations/second"

    The stats line may appear multiple times (progress updates); use last occurrence.
    """
    metrics = RunMetrics()

    # Try stdout first, then stderr (mutmut may write to either)
    for path in [stdout_path, stderr_path]:
        if not path or not path.exists():
            continue
        text = path.read_text(errors="replace")

        # Stats summary line: "X/Y  🎉 K ..."
        # The pattern: digits/digits followed by emoji counts
        for m in re.finditer(r"(\d+)/(\d+)\s+🎉\s*(\d+)", text):
            total = int(m.group(2))
            killed = int(m.group(3))
            metrics.total_mutants = total
            metrics.killed = killed

        # Survived: "🙁 N"
        for m in re.finditer(r"🙁\s*(\d+)", text):
            metrics.survived = int(m.group(1))

        # Mutations/sec: "N.NN mutations/second"
        for m in re.finditer(r"([\d.]+)\s+mutations/second", text):
            metrics.mutants_per_sec = float(m.group(1))

    return metrics


# ── Core aggregation ──────────────────────────────────────────────────────

def collect_config_summaries(
    result_dir: Path, ncpu: int, runs: int
) -> dict[str, ConfigSummary]:
    """Scan result_dir for run files and aggregate by config name."""

    configs: dict[str, ConfigSummary] = {}

    def get_or_create(config: str, label: str) -> ConfigSummary:
        if config not in configs:
            configs[config] = ConfigSummary(config=config, label=label)
        return configs[config]

    # Find all time files to discover which runs exist
    time_files = sorted(result_dir.glob("*_time.txt"))
    for time_file in time_files:
        stem = time_file.stem  # e.g. "irradiate_pool_8w_run1_time"
        # Strip trailing "_time"
        run_key = stem[: -len("_time")]  # e.g. "irradiate_pool_8w_run1"

        # Split off "_runN" suffix
        m = re.match(r"^(.+)_run(\d+)$", run_key)
        if not m:
            continue
        config = m.group(1)
        run_n = int(m.group(2))

        # Build human-readable label
        label = config_label(config, ncpu)
        summary = get_or_create(config, label)

        # Parse timing
        wall, rss_mb = parse_time_file(time_file)

        # Parse tool output
        stdout_f = result_dir / f"{config}_run{run_n}_stdout.txt"
        stderr_f = result_dir / f"{config}_run{run_n}_stderr.txt"

        if config.startswith("irradiate"):
            tool_metrics = parse_irradiate_stderr(stderr_f)
        else:
            tool_metrics = parse_mutmut_output(stdout_f, stderr_f)

        tool_metrics.wall_secs = wall
        tool_metrics.peak_rss_mb = rss_mb
        summary.runs.append(tool_metrics)

    return configs


def config_label(config: str, ncpu: int) -> str:
    """Map config key to display label."""
    labels = {
        f"irradiate_pool_{ncpu}w": f"irradiate pool ({ncpu}w)",
        "irradiate_pool_1w": "irradiate pool (1w)",
        "irradiate_isolate": "irradiate isolate",
        f"mutmut_{ncpu}c": f"mutmut ({ncpu}c)",
        "mutmut_1c": "mutmut (1c)",
    }
    return labels.get(config, config)


# ── Formatting ─────────────────────────────────────────────────────────────

def fmt_secs(val: float | None, mn: float | None = None, mx: float | None = None) -> str:
    if val is None:
        return "—"
    s = f"{val:.1f}"
    if mn is not None and mx is not None and abs(mx - mn) > 0.05:
        s += f" ({mn:.1f}–{mx:.1f})"
    return s


def fmt_mps(val: float | None) -> str:
    return f"{val:.1f}" if val is not None else "—"


def fmt_rss(val: float | None) -> str:
    return f"{val:.0f}" if val is not None else "—"


def fmt_score(val: float | None) -> str:
    return f"{val:.0f}%" if val is not None else "—"


def fmt_int(val: int | None) -> str:
    return str(val) if val is not None else "—"


# ── Output ─────────────────────────────────────────────────────────────────

ORDERED_CONFIG_PATTERNS = [
    r"irradiate_pool_\d+w",
    r"irradiate_pool_1w",
    r"irradiate_isolate",
    r"mutmut_\d+c",
    r"mutmut_1c",
]


def order_configs(configs: dict[str, ConfigSummary]) -> list[ConfigSummary]:
    ordered = []
    seen = set()
    for pat in ORDERED_CONFIG_PATTERNS:
        for key in sorted(configs.keys()):
            if re.fullmatch(pat, key) and key not in seen:
                ordered.append(configs[key])
                seen.add(key)
    # Append any remaining in original order
    for key, v in configs.items():
        if key not in seen:
            ordered.append(v)
    return ordered


def build_markdown_table(
    summaries: list[ConfigSummary], target: str, ncpu: int, runs: int
) -> str:
    lines = [
        f"# Benchmark: {target}",
        "",
        f"CPUs: {ncpu}  |  Runs per config: {runs} (plus 1 warmup discarded)",
        "",
        "| Configuration | Median Wall (s) | Mutants | Killed | Survived | Score | ms/mutant | Mut/s | Peak RSS (MB) |",
        "|---|---|---|---|---|---|---|---|---|",
    ]

    for s in summaries:
        label = s.label
        wall = fmt_secs(s.median_wall(), s.min_wall(), s.max_wall())
        mutants = fmt_int(s.consensus_mutants())
        killed = fmt_int(s.consensus_killed())
        survived = fmt_int(s.consensus_survived())
        score = fmt_score(s.mutation_score())
        ms_per_mut = f"{s.ms_per_mutant():.1f}" if s.ms_per_mutant() is not None else "—"
        mps = fmt_mps(s.median_mps())
        rss = fmt_rss(s.median_rss())
        lines.append(f"| {label} | {wall} | {mutants} | {killed} | {survived} | {score} | {ms_per_mut} | {mps} | {rss} |")

    lines += [
        "",
        "## Notes",
        "",
        "- Wall time is median of timed runs; range shown when spread > 50ms.",
        "- Peak RSS is median across runs (macOS `maximum resident set size`).",
        "- Mut/s is median mutations/second as reported by each tool.",
        "- Mutation score = killed / (killed + survived) × 100.",
        "- Mutant counts differ between tools due to operator coverage gaps.",
        "- Per-mutant time is the fairest comparison metric.",
        "- `irradiate isolate` is most comparable to mutmut (both spawn fresh processes).",
        "- `irradiate pool` uses persistent worker processes (lower overhead).",
        "",
    ]
    return "\n".join(lines)


def build_raw_data(
    summaries: list[ConfigSummary], target: str, ncpu: int, runs: int
) -> dict:
    return {
        "target": target,
        "ncpu": ncpu,
        "runs": runs,
        "configs": [
            {
                "config": s.config,
                "label": s.label,
                "median_wall_secs": s.median_wall(),
                "min_wall_secs": s.min_wall(),
                "max_wall_secs": s.max_wall(),
                "median_rss_mb": s.median_rss(),
                "mutants": s.consensus_mutants(),
                "killed": s.consensus_killed(),
                "survived": s.consensus_survived(),
                "mutation_score_pct": s.mutation_score(),
                "ms_per_mutant": s.ms_per_mutant(),
                "median_mps": s.median_mps(),
                "runs": [asdict(r) for r in s.runs],
            }
            for s in summaries
        ],
    }


# ── Main ──────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result_dir", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--ncpu", type=int, required=True)
    parser.add_argument("--runs", type=int, required=True)
    args = parser.parse_args()

    result_dir: Path = args.result_dir
    if not result_dir.is_dir():
        parser.error(f"result_dir does not exist: {result_dir}")

    configs = collect_config_summaries(result_dir, args.ncpu, args.runs)
    if not configs:
        print(f"Warning: no benchmark runs found in {result_dir}", flush=True)

    summaries = order_configs(configs)

    md = build_markdown_table(summaries, args.target, args.ncpu, args.runs)
    raw = build_raw_data(summaries, args.target, args.ncpu, args.runs)

    summary_path = result_dir / "summary.md"
    raw_path = result_dir / "raw_data.json"

    summary_path.write_text(md)
    raw_path.write_text(json.dumps(raw, indent=2))

    print(f"Written: {summary_path}")
    print(f"Written: {raw_path}")


if __name__ == "__main__":
    main()
