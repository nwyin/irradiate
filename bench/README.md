# irradiate benchmarks

Scripts to benchmark irradiate on shared Python test targets.

## Architecture comparison (apples-to-oranges)

This benchmark compares irradiate against **mutmut 2.5.1**. The two tools use
fundamentally different mutation architectures — the comparison is informative,
not a direct apples-to-apples speed contest.

### How each tool works

**mutmut 2.x — disk-based mutation**
- Rewrites mutated source to disk, runs pytest against the modified file,
  then restores the original. Each mutant incurs file I/O.
- Parses Python with parso (pure Python AST library).
- Parallelism via `--max-children N` (multiple pytest subprocesses, each testing
  one mutant at a time).

**irradiate — trampoline-based mutation**
- Compiles all function variants (original + all mutants) into a single file at
  start-up. Mutant switching is a global variable assignment — zero disk I/O per
  mutant.
- Parses Python with libcst (Rust-native, via pyo3).
- Parallelism via a persistent worker pool (workers stay alive across mutants,
  paying startup cost once per worker rather than once per mutant).

### What this means for the benchmark numbers

- **Disk I/O**: irradiate's trampoline eliminates per-mutant file writes — a
  structural advantage, not just an optimization.
- **Parsing speed**: libcst (Rust-native) vs parso (pure Python) differs in
  baseline parsing cost, though this is a one-time cost per file.
- **Mutant counts differ**: operator coverage is not identical between tools.
  Counts will NOT match. This is expected.
- **ms/mutant is the fairest metric**: it normalises for different mutant counts
  and lets you compare efficiency per unit of work.
- **mutmut 2.x ≠ mutmut 3.x**: mutmut 2.x is disk-based. mutmut 3.x introduced
  the trampoline architecture — but 3.x crashes on macOS (see below). This
  comparison is disk-based vs trampoline, not old-mutmut vs new-mutmut.

### Why mutmut 2.5.1 (not 3.x)

mutmut 3.0–3.5 all crash on macOS:
- Issue #466: `set_start_method` crash at startup
- Issue #446: `setproctitle` segfaults in forked workers

2.5.1 is the last stable release before the broken 3.x rewrite. The CLI
interface (`mutmut run --max-children N`) and output format are unchanged,
so existing result parsing works without modification.

## Setup

Run once from the project root:

```bash
bash bench/setup.sh
```

This will:
1. Build `target/release/irradiate` (release mode)
2. Create `tests/fixtures/simple_project/.venv`
3. Create `vendor/mutmut/e2e_projects/my_lib/.venv`
4. Create `bench/targets/synth/.venv`
5. Create `bench/.venv` with mutmut==2.5.1 + pytest (for benchmark comparison)

## Running benchmarks

```bash
# Benchmark simple_project (default 3 timed runs + 1 warmup)
bash bench/compare.sh simple_project

# Benchmark my_lib with 5 timed runs
bash bench/compare.sh my_lib --runs 5

# Override run count via env var
BENCH_RUNS=5 bash bench/compare.sh simple_project
```

Results are written to `bench/results/<timestamp>/<target>/`:
- `summary.md` — markdown table
- `raw_data.json` — structured data for further analysis
- `<config>_runN_{stdout,stderr,time}.txt` — raw tool output and timing

## Available targets

| Target | Location | Description |
|---|---|---|
| `simple_project` | `tests/fixtures/simple_project/` | Minimal irradiate fixture (3 functions, 3 tests) |
| `my_lib` | `vendor/mutmut/e2e_projects/my_lib/` | mutmut's own e2e fixture |
| `synth` | `bench/targets/synth/` | Synthetic utility library (~150 mutants) designed to show pool worker advantage |

## Configurations tested

| Config | Tool | Description |
|---|---|---|
| `irradiate pool (Nw)` | irradiate | Pool mode, all CPU cores |
| `irradiate pool (1w)` | irradiate | Pool mode, single worker |
| `irradiate isolate` | irradiate | Isolated subprocess per mutant |
| `mutmut (Nc)` | mutmut 2.5.1 | All CPU cores (`--max-children N`) |
| `mutmut (1c)` | mutmut 2.5.1 | Single child process (`--max-children 1`) |

## Measurement methodology

**Clean slate**: `mutants/`, `.irradiate/`, and `.mutmut-cache` are deleted before
every run to avoid result-caching effects.

**Warmup discarded**: One warmup run is performed before timed runs to warm OS disk
caches and JIT state. It is not included in the reported metrics.

**Timing method**: `/usr/bin/time -l` (macOS). This captures wall-clock time and peak
RSS without external dependencies. Per-process subprocess timing is not measured.

**Median reported**: The median of N timed runs is reported; min/max range is shown when
spread exceeds 50ms.

### What to compare

**ms/mutant (wall time / total mutants)** is the primary comparison metric between
irradiate and mutmut. It normalises for the fact that the two tools generate different
mutant counts (different operator coverage), making raw wall-clock time misleading.

**Mutation score** (killed / (killed + survived)) reflects operator coverage and test
suite quality. Scores will differ between tools because operator sets differ.

**Wall time** within a single tool (e.g., `irradiate pool Nw` vs `irradiate pool 1w`)
is a fair apples-to-apples comparison since both runs produce the same mutant set.

## Re-running summarize.py

If you want to regenerate the table from existing results without re-running the tools:

```bash
uv run --python 3.12 bench/summarize.py \
    bench/results/<timestamp>/simple_project \
    --target simple_project \
    --ncpu 8 \
    --runs 3
```

## Adding a new target

1. Create `bench/targets/<name>.sh` exporting `PROJECT_DIR`, `PATHS_TO_MUTATE`,
   `TESTS_DIR`, and `PYTHON`.
2. Ensure the project has `[tool.mutmut]` in its `pyproject.toml`.
3. Add any venv setup to `bench/setup.sh`.
4. Run: `bash bench/compare.sh <name>`
