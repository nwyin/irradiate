#!/usr/bin/env python3
"""Generate a static HTML report page for irradiate.

Four sections:
  1. Hero: project pitch, quick start, example output, nav links
  2. Benchmark results: comparison table from bench/results/ raw_data.json
  3. Vendor test results: mutation scores for markupsafe/click/httpx
  4. Architecture overview: parse → mutate → trampoline → test

Usage:
    python scripts/generate_report.py [-o report/index.html]
    python scripts/generate_report.py [-o report/index.html] [--bench-dir bench/results] [--vendor-results vendor_results.json]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import time
import warnings
from html import escape
from pathlib import Path

# ---------------------------------------------------------------------------
# Static content
# ---------------------------------------------------------------------------

EXAMPLE_OUTPUT = """\
$ irradiate run --paths-to-mutate src/mylib --tests-dir tests/

Generating mutants...  87 mutants across 3 files
Running mutation tests (8 workers)...

████████████████████░░░░  87/87

Mutation testing complete (87 mutants in 4.2s, 20 mutants/sec)

  Killed:    52  (59.8%)
  Survived:  23  (26.4%)
  No tests:  12  (13.8%)

Mutation score: 69.3%"""


# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------


def find_raw_data(bench_dir: Path) -> tuple[dict | None, str]:
    """Find the most recent raw_data.json under bench_dir.

    Searches bench_dir/<timestamp>/<target>/raw_data.json and
    bench_dir/<timestamp>/raw_data.json, picking lexicographically latest
    <timestamp> dir.

    Returns (data_dict, source_path_str) or (None, reason_string).
    """
    if not bench_dir.exists():
        return None, f"{bench_dir} does not exist"

    # List timestamp directories (lexicographic sort — YYYYMMDD_HHMMSS format)
    ts_dirs = sorted([d for d in bench_dir.iterdir() if d.is_dir()], reverse=True)
    if not ts_dirs:
        return None, f"no subdirectories in {bench_dir}"

    for ts_dir in ts_dirs:
        # Try depth-1: bench_dir/<ts>/<target>/raw_data.json
        candidates = sorted(ts_dir.glob("*/raw_data.json"), reverse=True)
        # Try depth-0: bench_dir/<ts>/raw_data.json
        candidates += [ts_dir / "raw_data.json"]

        for candidate in candidates:
            if candidate.exists():
                try:
                    data = json.loads(candidate.read_text())
                    return data, str(candidate)
                except (json.JSONDecodeError, OSError) as e:
                    warnings.warn(f"Skipping malformed {candidate}: {e}")
                    continue

    return None, "no raw_data.json found under any timestamp directory"


def load_vendor_results(path: Path | None) -> dict | None:
    """Load vendor test results JSON if available."""
    if path is None or not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except (json.JSONDecodeError, OSError) as e:
        warnings.warn(f"Could not load vendor results from {path}: {e}")
        return None


def get_version() -> str:
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            capture_output=True,
            timeout=30,
        )
        data = json.loads(proc.stdout.decode())
        for pkg in data.get("packages", []):
            if pkg["name"] == "irradiate":
                return pkg["version"]
    except Exception:
        pass
    return "unknown"


def get_git_sha() -> str:
    try:
        proc = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, timeout=10)
        return proc.stdout.decode().strip()
    except Exception:
        return "unknown"


# ---------------------------------------------------------------------------
# HTML helpers
# ---------------------------------------------------------------------------


def fmt_val(v: float | int | None, decimals: int = 1) -> str:
    if v is None:
        return "&mdash;"
    if isinstance(v, int):
        return str(v)
    return f"{v:.{decimals}f}"


def fmt_score(v: float | None) -> str:
    return f"{v:.1f}%" if v is not None else "&mdash;"


def fmt_ms(v: float | None) -> str:
    return f"{v:.1f}" if v is not None else "&mdash;"


def fmt_rss(v: float | None) -> str:
    return f"{v:.0f}" if v is not None else "&mdash;"


def fmt_wall_range(median: float | None, mn: float | None, mx: float | None) -> str:
    """Format wall time in ms, with optional range."""
    if median is None:
        return "&mdash;"
    med_ms, mn_ms, mx_ms = median * 1000, (mn or 0) * 1000, (mx or 0) * 1000
    s = f"{med_ms:,.0f}"
    if mn is not None and mx is not None and abs(mx_ms - mn_ms) > 50:
        s += f" ({mn_ms:,.0f}&ndash;{mx_ms:,.0f})"
    return s


def fmt_speedup(ms_per_mutant: float | None, baseline_ms: float | None) -> str:
    if ms_per_mutant is None or baseline_ms is None or ms_per_mutant == 0:
        return "&mdash;"
    ratio = baseline_ms / ms_per_mutant
    if abs(ratio - 1.0) < 0.05:
        return "1.0x"
    return f"{ratio:.1f}x"


def is_fastest(config: str, configs: list[dict]) -> bool:
    """True if this config has the lowest ms_per_mutant among all configs."""
    vals: list[tuple[float, str]] = []
    for c in configs:
        ms = c.get("ms_per_mutant")
        if ms is not None:
            vals.append((float(ms), c["config"]))
    if not vals:
        return False
    best_config = min(vals, key=lambda x: x[0])[1]
    return config == best_config


def mutmut_baseline_ms(configs: list[dict]) -> float | None:
    """Find the mutmut config's ms_per_mutant to use as speedup baseline."""
    for c in configs:
        if c.get("config", "").startswith("mutmut"):
            return c.get("ms_per_mutant")
    return None


# ---------------------------------------------------------------------------
# Section 1: Hero
# ---------------------------------------------------------------------------


def _hero_html() -> str:
    return f"""
<header class="hero">
  <h1>irradiate</h1>
  <p class="tagline">Mutation testing for Python, written in Rust.</p>
  <p class="tagline-sub">
    Spiritual successor to <a href="https://github.com/boxed/mutmut">mutmut</a>.
    Pre-warmed pytest worker pool, parallel mutation generation, structured results.
  </p>

  <nav class="page-nav">
    <a href="#get-started">Get started</a>
    <a href="#benchmarks">Benchmarks</a>
    <a href="#vendor-tests">Vendor tests</a>
    <a href="https://github.com/tau/irradiate">GitHub</a>
  </nav>

  <div class="section" id="get-started">
    <h2>Get started</h2>
    <pre class="code-block"><span class="prompt">$</span> git clone https://github.com/tau/irradiate &amp;&amp; cd irradiate
<span class="prompt">$</span> cargo build --release
<span class="prompt">$</span> irradiate run --paths-to-mutate src/mylib --tests-dir tests/</pre>
  </div>

  <div class="section" id="example">
    <h2>What it looks like</h2>
    <pre class="code-block">{escape(EXAMPLE_OUTPUT)}</pre>
  </div>

  <div class="why-box">
    <h3>Why irradiate?</h3>
    <ul>
      <li><strong>Worker pool:</strong> pytest processes start once and are reused across mutants — no per-mutant Python startup overhead.</li>
      <li><strong>Parallel mutation generation:</strong> tree-sitter-based parsing runs across all source files in parallel via rayon.</li>
      <li><strong>Targeted tests:</strong> stats mode identifies which tests cover each function, so only relevant tests run per mutant.</li>
      <li><strong>Compatible results:</strong> follows mutmut naming conventions; mutation scores and metadata stored as JSON for tooling.</li>
    </ul>
  </div>
</header>"""


# ---------------------------------------------------------------------------
# Section 2: Benchmarks
# ---------------------------------------------------------------------------


def _benchmarks_html(bench_dir: Path) -> str:
    data, source = find_raw_data(bench_dir)

    if data is None:
        return f"""
<section class="section" id="benchmarks">
  <h2>Benchmark results</h2>
  <p class="section-desc">No benchmark data available yet. Run <code>bash bench/compare.sh synth</code> to generate results.</p>
  <p class="placeholder-note">Placeholder: data will appear here after the first benchmark run. ({escape(source)})</p>
</section>"""

    configs = data.get("configs", [])
    target = data.get("target", "unknown")
    ncpu = data.get("ncpu", "?")
    runs = data.get("runs", "?")
    baseline_ms = mutmut_baseline_ms(configs)

    rows = ""
    for cfg in configs:
        config_key = cfg.get("config", "")
        label = escape(cfg.get("label", config_key))
        wall = fmt_wall_range(cfg.get("median_wall_secs"), cfg.get("min_wall_secs"), cfg.get("max_wall_secs"))
        mutants = fmt_val(cfg.get("mutants"))
        killed = fmt_val(cfg.get("killed"))
        survived = fmt_val(cfg.get("survived"))
        score = fmt_score(cfg.get("mutation_score_pct"))
        ms_per_mut = fmt_ms(cfg.get("ms_per_mutant"))
        mps = fmt_val(cfg.get("median_mps"), 1)
        rss = fmt_rss(cfg.get("median_rss_mb"))
        speedup = fmt_speedup(cfg.get("ms_per_mutant"), baseline_ms)

        highlight = is_fastest(config_key, configs)
        row_class = ' class="row-highlight"' if highlight else ""
        label_cell = f'<td class="config-label highlight">{label} <span class="fastest-badge">fastest</span></td>' if highlight else f'<td class="config-label">{label}</td>'

        rows += f"""<tr{row_class}>
  {label_cell}
  <td>{wall}</td>
  <td>{mutants}</td>
  <td>{killed}</td>
  <td>{survived}</td>
  <td>{score}</td>
  <td>{ms_per_mut}</td>
  <td>{mps}</td>
  <td>{speedup}</td>
  <td>{rss}</td>
</tr>"""

    return f"""
<section class="section" id="benchmarks">
  <h2>Benchmark results</h2>
  <p class="section-desc">
    Target: <code>{escape(target)}</code> &middot;
    CPUs: {ncpu} &middot;
    Runs: {runs} (plus 1 warmup discarded) &middot;
    Source: <code>{escape(source)}</code>
  </p>
  <p class="section-desc">
    ms/mutant is the fairest comparison metric &mdash; it normalizes for operator coverage differences between tools.
    Speedup is relative to mutmut&rsquo;s ms/mutant.
    irradiate pool mode uses persistent pre-warmed pytest workers; isolate mode is most comparable to mutmut.
  </p>
  <div class="table-wrap">
    <table class="bench-table">
      <thead>
        <tr>
          <th>Configuration</th>
          <th>Wall (ms)</th>
          <th>Mutants</th>
          <th>Killed</th>
          <th>Survived</th>
          <th>Score</th>
          <th>ms/mutant</th>
          <th>Mut/s</th>
          <th>Speedup</th>
          <th>Peak RSS (MB)</th>
        </tr>
      </thead>
      <tbody>
        {rows}
      </tbody>
    </table>
  </div>
  <p class="bench-note">
    Note: the benchmark pins mutmut to the version configured in <code>bench/setup.sh</code>.
    The <code>mutmut (1c)</code> row is forced to a single child via <code>--max-children 1</code>.
    irradiate pool mode runs multiple pre-warmed pytest workers in parallel;
    irradiate isolate mode spawns a fresh process per mutant (most comparable to mutmut).
    Mutant counts differ between tools due to operator coverage gaps.
    Wall time range (min&ndash;max) shown when spread exceeds 50ms.
  </p>
</section>"""


# ---------------------------------------------------------------------------
# Section 3: Vendor tests
# ---------------------------------------------------------------------------

VENDOR_REPOS = [
    ("markupsafe", "https://github.com/pallets/markupsafe"),
    ("click", "https://github.com/pallets/click"),
    ("httpx", "https://github.com/encode/httpx"),
]


def _vendor_tests_html(vendor_data: dict | None) -> str:
    if vendor_data is None:
        repo_rows = ""
        for name, url in VENDOR_REPOS:
            repo_rows += f"""
<tr>
  <td><a href="{url}" target="_blank">{escape(name)}</a></td>
  <td><span class="badge badge-pending">not yet tested</span></td>
  <td>&mdash;</td>
  <td>&mdash;</td>
  <td>&mdash;</td>
  <td>&mdash;</td>
</tr>"""

        return f"""
<section class="section" id="vendor-tests">
  <h2>Vendor test results</h2>
  <p class="section-desc">
    Smoke tests against popular open-source Python libraries to verify irradiate works on real-world code.
    Run <code>bash tests/vendor_test.sh</code> to populate this section.
  </p>
  <div class="table-wrap">
    <table class="vendor-table">
      <thead>
        <tr>
          <th>Project</th>
          <th>Status</th>
          <th>Mutants</th>
          <th>Killed</th>
          <th>Survived</th>
          <th>Score</th>
        </tr>
      </thead>
      <tbody>{repo_rows}
      </tbody>
    </table>
  </div>
</section>"""

    # Render from vendor_data if available
    repo_rows = ""
    repos = vendor_data.get("repos", [])
    for repo in repos:
        name = repo.get("name", "?")
        url = next((u for n, u in VENDOR_REPOS if n == name), "#")
        status = repo.get("status", "unknown")
        mutants = repo.get("mutants")
        killed = repo.get("killed")
        survived = repo.get("survived")
        score = repo.get("mutation_score_pct")

        badge_class = "badge-pass" if status == "pass" else ("badge-fail" if status == "fail" else "badge-pending")
        repo_rows += f"""
<tr>
  <td><a href="{url}" target="_blank">{escape(name)}</a></td>
  <td><span class="badge {badge_class}">{escape(status)}</span></td>
  <td>{fmt_val(mutants) if mutants is not None else "&mdash;"}</td>
  <td>{fmt_val(killed) if killed is not None else "&mdash;"}</td>
  <td>{fmt_val(survived) if survived is not None else "&mdash;"}</td>
  <td>{fmt_score(score)}</td>
</tr>"""

    run_date = vendor_data.get("run_date", "unknown")
    return f"""
<section class="section" id="vendor-tests">
  <h2>Vendor test results</h2>
  <p class="section-desc">
    Smoke tests against popular open-source Python libraries. Run date: {escape(run_date)}.
  </p>
  <div class="table-wrap">
    <table class="vendor-table">
      <thead>
        <tr>
          <th>Project</th>
          <th>Status</th>
          <th>Mutants</th>
          <th>Killed</th>
          <th>Survived</th>
          <th>Score</th>
        </tr>
      </thead>
      <tbody>{repo_rows}
      </tbody>
    </table>
  </div>
</section>"""


# ---------------------------------------------------------------------------
# CSS
# ---------------------------------------------------------------------------

CSS = """
:root {
    --bg: #0d1117;
    --surface: #161b22;
    --surface2: #1c2129;
    --border: #30363d;
    --text: #e6edf3;
    --text-muted: #8b949e;
    --accent: #58a6ff;
    --green: #3fb950;
    --orange: #d29922;
    --red: #f85149;
    --yellow: #e3b341;
    --font: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    --mono: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
    font-family: var(--font);
    background: var(--bg);
    color: var(--text);
    line-height: 1.6;
    padding: 2rem;
    max-width: 1100px;
    margin: 0 auto;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }

/* Hero */
.hero { margin-bottom: 3rem; }
h1 { font-size: 2.25rem; margin-bottom: 0.25rem; }
h3 { font-size: 1rem; font-weight: 600; margin-bottom: 0.5rem; }
.tagline { font-size: 1.125rem; color: var(--text); margin-bottom: 0.125rem; }
.tagline-sub { font-size: 0.9375rem; color: var(--text-muted); margin-bottom: 1.5rem; }
.tagline-sub a { color: var(--text-muted); }
.tagline-sub a:hover { color: var(--accent); }

/* Nav */
.page-nav { margin-bottom: 2rem; display: flex; gap: 1.5rem; font-size: 0.875rem; }
.page-nav a { color: var(--text-muted); }
.page-nav a:hover { color: var(--accent); }

/* Code blocks */
.code-block {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 1rem;
    font-family: var(--mono);
    font-size: 0.8125rem;
    line-height: 1.6;
    overflow-x: auto;
    white-space: pre;
}
.code-block .prompt { color: var(--text-muted); }

/* Why box */
.why-box {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 1rem 1.25rem;
    margin-top: 1.5rem;
}
.why-box ul {
    list-style: none;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    font-size: 0.9rem;
    color: var(--text-muted);
}
.why-box li::before { content: "→ "; color: var(--accent); }
.why-box strong { color: var(--text); }

/* Sections */
.section { margin-top: 2.5rem; }
.section h2 {
    font-size: 1.125rem;
    margin-bottom: 0.5rem;
    padding-bottom: 0.5rem;
    border-bottom: 1px solid var(--border);
}
.section-desc {
    color: var(--text-muted);
    font-size: 0.8125rem;
    margin-bottom: 0.75rem;
    line-height: 1.6;
}
.section-desc a { color: var(--accent); }
.section-desc code {
    font-family: var(--mono);
    font-size: 0.85em;
    background: var(--surface);
    padding: 0.1em 0.35em;
    border-radius: 3px;
}

/* Placeholder */
.placeholder-note {
    color: var(--text-muted);
    font-size: 0.8rem;
    font-family: var(--mono);
    margin-top: 0.5rem;
}

/* Tables */
.table-wrap { overflow-x: auto; }
table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.875rem;
    margin-bottom: 0.75rem;
}
th, td {
    padding: 0.5rem 0.75rem;
    text-align: left;
    border-bottom: 1px solid var(--border);
}
th {
    color: var(--text-muted);
    font-weight: 500;
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    white-space: nowrap;
    background: var(--surface);
}
td { font-family: var(--mono); font-size: 0.8125rem; }
tr:hover { background: rgba(88,166,255,0.04); }
.row-highlight { background: rgba(63,185,80,0.05); }
.row-highlight:hover { background: rgba(63,185,80,0.08); }

/* Bench table */
.config-label { font-family: var(--mono); font-weight: 500; }
.config-label.highlight { color: var(--green); }
.fastest-badge {
    font-size: 0.65rem;
    padding: 0.1rem 0.4rem;
    border-radius: 8px;
    background: rgba(63,185,80,0.15);
    color: var(--green);
    font-weight: 600;
    vertical-align: middle;
    margin-left: 0.4rem;
    font-family: var(--font);
}
.bench-note {
    color: var(--text-muted);
    font-size: 0.8rem;
    margin-top: 0.5rem;
}
.bench-table td:nth-child(9) { font-weight: 600; }

/* Badges */
.badge {
    font-size: 0.75rem;
    padding: 0.15rem 0.5rem;
    border-radius: 10px;
    font-weight: 600;
    font-family: var(--font);
}
.badge-pass { background: rgba(63,185,80,0.15); color: var(--green); }
.badge-fail { background: rgba(248,81,73,0.15); color: var(--red); }
.badge-pending { background: rgba(139,148,158,0.15); color: var(--text-muted); }

/* Footer */
footer {
    margin-top: 3rem;
    padding-top: 1rem;
    border-top: 1px solid var(--border);
    color: var(--text-muted);
    font-size: 0.75rem;
}
footer a { color: var(--accent); }
footer code { font-family: var(--mono); }
"""


# ---------------------------------------------------------------------------
# Full page assembly
# ---------------------------------------------------------------------------


def generate_html(bench_dir: Path, vendor_data: dict | None, version: str, sha: str) -> str:
    now = time.strftime("%Y-%m-%d %H:%M UTC", time.gmtime())

    hero = _hero_html()
    benchmarks = _benchmarks_html(bench_dir)
    vendor = _vendor_tests_html(vendor_data)

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>irradiate &mdash; Mutation testing for Python, written in Rust</title>
<style>
{CSS}
</style>
</head>
<body>

{hero}
{benchmarks}
{vendor}

<footer>
  <a href="https://github.com/tau/irradiate">irradiate</a> v{version}
  &middot; commit <code>{sha}</code>
  &middot; generated {now}
</footer>

</body>
</html>"""


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate irradiate static HTML report")
    parser.add_argument("-o", "--output", default="report/index.html", help="Output HTML file (default: report/index.html)")
    parser.add_argument("--bench-dir", default="bench/results", type=Path, help="Benchmark results directory (default: bench/results)")
    parser.add_argument("--vendor-results", default=None, type=Path, help="Vendor test results JSON file (optional)")
    args = parser.parse_args()

    bench_dir: Path = args.bench_dir
    vendor_path: Path | None = args.vendor_results

    print(f"Bench dir:  {bench_dir}")
    print(f"Vendor:     {vendor_path or '(none)'}")

    version = get_version()
    sha = get_git_sha()
    print(f"Version:    {version}, commit: {sha}")

    vendor_data = load_vendor_results(vendor_path)
    if vendor_data:
        print(f"Vendor data loaded: {len(vendor_data.get('repos', []))} repos")
    else:
        print("Vendor data: not available (will show placeholders)")

    html = generate_html(bench_dir, vendor_data, version, sha)

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(html, encoding="utf-8")

    size_kb = len(html.encode()) / 1024
    print(f"\nReport written to {output_path} ({size_kb:.1f} KB)")


if __name__ == "__main__":
    main()
