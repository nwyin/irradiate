#!/usr/bin/env python3
"""
bench/summarize.py — Parse benchmark run results and produce markdown tables.

Per-target mode (default):
    python bench/summarize.py <result_dir> --target <name> --ncpu N --runs N

Aggregate mode (cross-target comparison):
    python bench/summarize.py <parent_dir> --aggregate
"""

from __future__ import annotations

import argparse
import json
import math
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


@dataclass
class TargetData:
    """Aggregated data for one target across all configs."""

    name: str
    configs: dict[str, ConfigSummary] = field(default_factory=dict)
    operator_breakdown: dict[str, dict] | None = None  # from Stryker report


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

    mutmut 2.x (--simple-output or default) uses text progress lines:
        "144/144  KILLED 59  TIMEOUT 0  SUSPICIOUS 0  SURVIVED 85  SKIPPED 0"
    mutmut 3.x uses emoji-delimited counts:
        "87/87  🎉 52 🫥 0  🔊 0  🤔 0  🙁 23  🔇 0  🧙 0"

    The stats line may appear multiple times (progress updates); use last occurrence.
    """
    _KILLED_EMOJI = "\U0001f389"  # party popper (mutmut 3.x)
    _SURVIVED_EMOJI = "\U0001f641"  # slightly frowning face (mutmut 3.x)

    metrics = RunMetrics()

    for path in [stdout_path, stderr_path]:
        if not path or not path.exists():
            continue
        text = path.read_text(errors="replace")

        # mutmut 2.x text format: "N/M  KILLED K  TIMEOUT T  SUSPICIOUS S  SURVIVED V  SKIPPED X"
        for m in re.finditer(r"(\d+)/(\d+)\s+KILLED\s+(\d+)", text):
            metrics.total_mutants = int(m.group(2))
            metrics.killed = int(m.group(3))
        for m in re.finditer(r"SURVIVED\s+(\d+)", text):
            metrics.survived = int(m.group(1))

        # mutmut 3.x emoji format
        for m in re.finditer(rf"(\d+)/(\d+)\s+{_KILLED_EMOJI}\s*(\d+)", text):
            metrics.total_mutants = int(m.group(2))
            metrics.killed = int(m.group(3))
        for m in re.finditer(rf"{_SURVIVED_EMOJI}\s*(\d+)", text):
            metrics.survived = int(m.group(1))

        for m in re.finditer(r"([\d.]+)\s+mutations/second", text):
            metrics.mutants_per_sec = float(m.group(1))

    return metrics


def parse_stryker_report(path: Path) -> dict[str, dict] | None:
    """
    Parse a Stryker mutation-testing-report-schema v2 JSON file.

    Returns a dict mapping operator name to {total, killed, survived} counts,
    or None if the file doesn't exist or can't be parsed.
    """
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
    except (json.JSONDecodeError, OSError):
        return None

    operators: dict[str, dict] = {}
    files = data.get("files", {})
    for file_info in files.values():
        for mutant in file_info.get("mutants", []):
            op = mutant.get("mutatorName", "unknown")
            if op not in operators:
                operators[op] = {"total": 0, "killed": 0, "survived": 0}
            operators[op]["total"] += 1
            status = mutant.get("status", "")
            if status in ("Killed", "Timeout"):
                operators[op]["killed"] += 1
            elif status == "Survived":
                operators[op]["survived"] += 1
    return operators if operators else None


# ── Core aggregation ──────────────────────────────────────────────────────


def collect_config_summaries(result_dir: Path, ncpu: int, runs: int) -> dict[str, ConfigSummary]:
    """Scan result_dir for run files and aggregate by config name."""

    configs: dict[str, ConfigSummary] = {}

    def get_or_create(config: str, label: str) -> ConfigSummary:
        if config not in configs:
            configs[config] = ConfigSummary(config=config, label=label)
        return configs[config]

    time_files = sorted(result_dir.glob("*_time.txt"))
    for time_file in time_files:
        stem = time_file.stem
        run_key = stem[: -len("_time")]

        m = re.match(r"^(.+)_run(\d+)$", run_key)
        if not m:
            continue
        config = m.group(1)
        run_n = int(m.group(2))

        label = config_label(config, ncpu)
        summary = get_or_create(config, label)

        wall, rss_mb = parse_time_file(time_file)

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


def detect_ncpu(configs: dict[str, ConfigSummary]) -> int:
    """Infer CPU count from config names like irradiate_pool_10w."""
    for key in configs:
        m = re.match(r"irradiate_pool_(\d+)w", key)
        if m and m.group(1) != "1":
            return int(m.group(1))
    for key in configs:
        m = re.match(r"mutmut_(\d+)c", key)
        if m and m.group(1) != "1":
            return int(m.group(1))
    return 1


def config_label(config: str, ncpu: int) -> str:
    """Map config key to display label."""
    labels = {
        f"irradiate_pool_{ncpu}w": f"irradiate pool ({ncpu}w)",
        "irradiate_pool_1w": "irradiate pool (1w)",
        "irradiate_isolate": "irradiate isolate",
        "mutmut": "mutmut 3.5.0",
        # Legacy config names from older runs
        f"mutmut_{ncpu}c": f"mutmut ({ncpu}c)",
        "mutmut_1c": "mutmut 3.5.0",
    }
    return labels.get(config, config)


# ── Formatting ─────────────────────────────────────────────────────────────


def fmt_secs(val: float | None, mn: float | None = None, mx: float | None = None) -> str:
    if val is None:
        return "\u2014"
    s = f"{val:.1f}"
    if mn is not None and mx is not None and abs(mx - mn) > 0.05:
        s += f" ({mn:.1f}\u2013{mx:.1f})"
    return s


def fmt_mps(val: float | None) -> str:
    return f"{val:.1f}" if val is not None else "\u2014"


def fmt_rss(val: float | None) -> str:
    return f"{val:.0f}" if val is not None else "\u2014"


def fmt_score(val: float | None) -> str:
    return f"{val:.0f}%" if val is not None else "\u2014"


def fmt_int(val: int | None) -> str:
    return str(val) if val is not None else "\u2014"


def fmt_speedup(val: float | None) -> str:
    return f"{val:.1f}x" if val is not None else "\u2014"


# ── Per-target output ─────────────────────────────────────────────────────

ORDERED_CONFIG_PATTERNS = [
    r"irradiate_pool_\d+w",
    r"irradiate_pool_1w",
    r"irradiate_isolate",
    r"mutmut",
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
    for key, v in configs.items():
        if key not in seen:
            ordered.append(v)
    return ordered


def build_markdown_table(summaries: list[ConfigSummary], target: str, ncpu: int, runs: int) -> str:
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
        ms_per_mut = f"{s.ms_per_mutant():.1f}" if s.ms_per_mutant() is not None else "\u2014"
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
        "- Mutation score = killed / (killed + survived) x 100.",
        "- Mutant counts differ between tools due to operator coverage gaps.",
        "- Per-mutant time is the fairest comparison metric.",
        "- `irradiate isolate` is most comparable to mutmut (both spawn fresh processes).",
        "- `irradiate pool` uses persistent worker processes (lower overhead).",
        "",
    ]
    return "\n".join(lines)


def build_raw_data(summaries: list[ConfigSummary], target: str, ncpu: int, runs: int) -> dict:
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


# ── Aggregate output ──────────────────────────────────────────────────────


def find_config(configs: dict[str, ConfigSummary], pattern: str) -> ConfigSummary | None:
    """Find first config matching a regex pattern."""
    for key, cs in configs.items():
        if re.fullmatch(pattern, key):
            return cs
    return None


def compute_speedup(slow: ConfigSummary | None, fast: ConfigSummary | None) -> float | None:
    """Compute speedup ratio = slow / fast (both median wall times)."""
    if slow is None or fast is None:
        return None
    s = slow.median_wall()
    f = fast.median_wall()
    if s is None or f is None or f == 0:
        return None
    return s / f


def geometric_mean(values: list[float]) -> float | None:
    """Geometric mean of positive values."""
    if not values:
        return None
    return math.exp(sum(math.log(v) for v in values) / len(values))


def collect_all_targets(parent_dir: Path) -> list[TargetData]:
    """Discover target subdirectories and collect data for each."""
    targets = []
    for sub in sorted(parent_dir.iterdir()):
        if not sub.is_dir():
            continue
        time_files = list(sub.glob("*_time.txt"))
        if not time_files:
            continue

        # Detect ncpu from config names
        configs_raw: dict[str, ConfigSummary] = {}
        # First pass: discover configs to detect ncpu
        for tf in time_files:
            stem = tf.stem
            run_key = stem[: -len("_time")]
            m = re.match(r"^(.+)_run(\d+)$", run_key)
            if m:
                cfg = m.group(1)
                if cfg not in configs_raw:
                    configs_raw[cfg] = ConfigSummary(config=cfg, label=cfg)

        ncpu = detect_ncpu(configs_raw)
        runs = max(
            (len(list(sub.glob(f"{cfg}_run*_time.txt"))) for cfg in configs_raw),
            default=1,
        )

        configs = collect_config_summaries(sub, ncpu, runs)

        td = TargetData(name=sub.name, configs=configs)

        # Look for Stryker report from any irradiate config
        for report_file in sorted(sub.glob("irradiate_*_report.json")):
            td.operator_breakdown = parse_stryker_report(report_file)
            if td.operator_breakdown:
                break

        targets.append(td)

    return targets


def build_aggregate_markdown(targets: list[TargetData]) -> str:
    if not targets:
        return "# No benchmark data found.\n"

    # Detect ncpu from first target with multi-worker config
    ncpu = 1
    for td in targets:
        ncpu = detect_ncpu(td.configs)
        if ncpu > 1:
            break

    lines = [
        "# irradiate vs mutmut -- Benchmark Results",
        "",
        f"CPUs: {ncpu}  |  Targets: {len(targets)}  |  mutmut 3.5.0",
        "",
    ]

    # ── Speed comparison table ──
    lines += [
        "## Speed",
        "",
        "| Target | irradiate (Nw) | mutmut (Nc) | Speedup | irradiate (1w) | mutmut (1c) | Speedup |",
        "|---|---|---|---|---|---|---|",
    ]

    parallel_speedups: list[float] = []
    serial_speedups: list[float] = []

    for td in targets:
        irr_nw = find_config(td.configs, r"irradiate_pool_\d+w")
        if irr_nw and irr_nw.config == "irradiate_pool_1w":
            irr_nw = None
        irr_1w = find_config(td.configs, r"irradiate_pool_1w")
        mm_nc = find_config(td.configs, r"mutmut_\d+c")
        if mm_nc and mm_nc.config == "mutmut_1c":
            mm_nc = None
        mm_1c = find_config(td.configs, r"mutmut_1c") or find_config(td.configs, r"mutmut")

        sp_parallel = compute_speedup(mm_nc, irr_nw)
        sp_serial = compute_speedup(mm_1c, irr_1w)

        if sp_parallel is not None:
            parallel_speedups.append(sp_parallel)
        if sp_serial is not None:
            serial_speedups.append(sp_serial)

        irr_nw_wall = fmt_secs(irr_nw.median_wall()) if irr_nw else "\u2014"
        mm_nc_wall = fmt_secs(mm_nc.median_wall()) if mm_nc else "\u2014"
        irr_1w_wall = fmt_secs(irr_1w.median_wall()) if irr_1w else "\u2014"
        mm_1c_wall = fmt_secs(mm_1c.median_wall()) if mm_1c else "\u2014"

        lines.append(
            f"| {td.name} | {irr_nw_wall}s | {mm_nc_wall}s | {fmt_speedup(sp_parallel)} "
            f"| {irr_1w_wall}s | {mm_1c_wall}s | {fmt_speedup(sp_serial)} |"
        )

    gm_parallel = geometric_mean(parallel_speedups)
    gm_serial = geometric_mean(serial_speedups)
    lines.append(
        f"| **Geometric mean** | | | **{fmt_speedup(gm_parallel)}** " f"| | | **{fmt_speedup(gm_serial)}** |"
    )
    lines.append("")

    # ── Correctness comparison table ──
    lines += [
        "## Correctness",
        "",
        "| Target | Mutants (irr) | Mutants (mm) | Score (irr) | Score (mm) | Delta |",
        "|---|---|---|---|---|---|",
    ]

    irr_scores: list[float] = []
    mm_scores: list[float] = []

    for td in targets:
        # Use pool Nw for irradiate stats (same mutant set regardless of config)
        irr = find_config(td.configs, r"irradiate_pool_\d+w")
        if irr and irr.config == "irradiate_pool_1w":
            irr = find_config(td.configs, r"irradiate_pool_1w")
        mm = find_config(td.configs, r"mutmut_1c") or find_config(td.configs, r"mutmut")

        irr_mutants = irr.consensus_mutants() if irr else None
        mm_mutants = mm.consensus_mutants() if mm else None
        irr_score = irr.mutation_score() if irr else None
        mm_score = mm.mutation_score() if mm else None

        if irr_score is not None:
            irr_scores.append(irr_score)
        if mm_score is not None:
            mm_scores.append(mm_score)

        delta = "\u2014"
        if irr_score is not None and mm_score is not None:
            d = irr_score - mm_score
            delta = f"{d:+.0f}pp"

        lines.append(
            f"| {td.name} | {fmt_int(irr_mutants)} | {fmt_int(mm_mutants)} | {fmt_score(irr_score)} | {fmt_score(mm_score)} | {delta} |"
        )

    avg_irr = statistics.mean(irr_scores) if irr_scores else None
    avg_mm = statistics.mean(mm_scores) if mm_scores else None
    avg_delta = "\u2014"
    if avg_irr is not None and avg_mm is not None:
        avg_delta = f"{avg_irr - avg_mm:+.0f}pp"
    lines.append(f"| **Average** | | | {fmt_score(avg_irr)} | {fmt_score(avg_mm)} | {avg_delta} |")
    lines.append("")

    # ── Operator coverage (if Stryker reports available) ──
    any_ops = any(td.operator_breakdown for td in targets)
    if any_ops:
        # Aggregate operator counts across all targets
        all_ops: dict[str, dict] = {}
        for td in targets:
            if not td.operator_breakdown:
                continue
            for op, counts in td.operator_breakdown.items():
                if op not in all_ops:
                    all_ops[op] = {"total": 0, "killed": 0, "survived": 0}
                all_ops[op]["total"] += counts["total"]
                all_ops[op]["killed"] += counts["killed"]
                all_ops[op]["survived"] += counts["survived"]

        if all_ops:
            lines += [
                "## Operator Coverage (irradiate)",
                "",
                "| Operator | Mutants | Killed | Survived | Kill Rate |",
                "|---|---|---|---|---|",
            ]
            for op in sorted(all_ops, key=lambda o: all_ops[o]["total"], reverse=True):
                c = all_ops[op]
                rate = c["killed"] / c["total"] * 100 if c["total"] > 0 else 0
                lines.append(f"| {op} | {c['total']} | {c['killed']} | {c['survived']} | {rate:.0f}% |")
            total_all = sum(c["total"] for c in all_ops.values())
            lines.append(f"| **Total** | **{total_all}** | | | |")
            lines.append("")

    # ── Methodology notes ──
    lines += [
        "## Methodology",
        "",
        "- Each configuration ran with 1 warmup (discarded) + N timed runs.",
        "- Wall time is the median of timed runs.",
        "- Speedup = mutmut wall time / irradiate wall time.",
        "- Geometric mean is the correct aggregation for speedup ratios.",
        "- Mutation score = killed / (killed + survived) x 100.",
        "- Mutant counts differ between tools (different operator sets).",
        "- Delta = irradiate score minus mutmut score (positive = irradiate detects more).",
        "",
    ]
    return "\n".join(lines)


def build_aggregate_json(targets: list[TargetData]) -> dict:
    ncpu = 1
    for td in targets:
        ncpu = detect_ncpu(td.configs)
        if ncpu > 1:
            break

    target_data = []
    for td in targets:
        irr_nw = find_config(td.configs, r"irradiate_pool_\d+w")
        if irr_nw and irr_nw.config == "irradiate_pool_1w":
            irr_nw = None
        irr_1w = find_config(td.configs, r"irradiate_pool_1w")
        mm_nc = find_config(td.configs, r"mutmut_\d+c")
        if mm_nc and mm_nc.config == "mutmut_1c":
            mm_nc = None
        mm_1c = find_config(td.configs, r"mutmut_1c") or find_config(td.configs, r"mutmut")

        def config_summary(cs: ConfigSummary | None) -> dict | None:
            if cs is None:
                return None
            return {
                "config": cs.config,
                "median_wall_secs": cs.median_wall(),
                "mutants": cs.consensus_mutants(),
                "killed": cs.consensus_killed(),
                "survived": cs.consensus_survived(),
                "mutation_score_pct": cs.mutation_score(),
                "ms_per_mutant": cs.ms_per_mutant(),
            }

        target_data.append(
            {
                "target": td.name,
                "irradiate_parallel": config_summary(irr_nw),
                "irradiate_serial": config_summary(irr_1w),
                "mutmut_parallel": config_summary(mm_nc),
                "mutmut_serial": config_summary(mm_1c),
                "speedup_parallel": compute_speedup(mm_nc, irr_nw),
                "speedup_serial": compute_speedup(mm_1c, irr_1w),
                "operator_breakdown": td.operator_breakdown,
            }
        )

    parallel_speedups = [t["speedup_parallel"] for t in target_data if t["speedup_parallel"]]
    serial_speedups = [t["speedup_serial"] for t in target_data if t["speedup_serial"]]

    return {
        "ncpu": ncpu,
        "targets": target_data,
        "summary": {
            "geo_mean_speedup_parallel": geometric_mean(parallel_speedups),
            "geo_mean_speedup_serial": geometric_mean(serial_speedups),
            "target_count": len(targets),
        },
    }


# ── Main ──────────────────────────────────────────────────────────────────


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result_dir", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--ncpu", type=int)
    parser.add_argument("--runs", type=int)
    parser.add_argument("--aggregate", action="store_true", help="Aggregate across target subdirectories")
    args = parser.parse_args()

    result_dir: Path = args.result_dir
    if not result_dir.is_dir():
        parser.error(f"result_dir does not exist: {result_dir}")

    if args.aggregate:
        # Aggregate mode: result_dir is the parent containing target subdirs
        targets = collect_all_targets(result_dir)
        if not targets:
            print(f"Warning: no benchmark data found in {result_dir}", flush=True)
            return

        # Write per-target summaries
        for td in targets:
            sub = result_dir / td.name
            ncpu = detect_ncpu(td.configs)
            runs = max((len(cs.runs) for cs in td.configs.values()), default=1)
            summaries = order_configs(td.configs)
            md = build_markdown_table(summaries, td.name, ncpu, runs)
            raw = build_raw_data(summaries, td.name, ncpu, runs)
            (sub / "summary.md").write_text(md)
            (sub / "raw_data.json").write_text(json.dumps(raw, indent=2))
            print(f"Written: {sub}/summary.md")

        # Write aggregate report
        agg_md = build_aggregate_markdown(targets)
        agg_json = build_aggregate_json(targets)
        (result_dir / "aggregate.md").write_text(agg_md)
        (result_dir / "aggregate.json").write_text(json.dumps(agg_json, indent=2))
        print(f"Written: {result_dir}/aggregate.md")
        print(f"Written: {result_dir}/aggregate.json")

    else:
        # Per-target mode (original behavior)
        if not args.target or args.ncpu is None or args.runs is None:
            parser.error("--target, --ncpu, and --runs are required in per-target mode")

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
