# irradiate benchmarks

Scripts to benchmark irradiate on shared Python test targets.

<!-- TODO: re-add mutmut comparison when upstream fixes v3 bugs
     (set_start_method crash #466, fork+setproctitle segfaults #446,
      trampoline codegen bugs #387/#480/#477) -->

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

| Config | Description |
|---|---|
| `irradiate pool (Nw)` | irradiate pool mode, all CPU cores |
| `irradiate pool (1w)` | irradiate pool mode, single worker |
| `irradiate isolate` | irradiate isolated subprocess mode |

<!-- TODO: re-add mutmut configs when upstream fixes v3 bugs -->
<!-- | `mutmut (Nc)` | mutmut, all CPU cores (`--max-children N`) | -->
<!-- | `mutmut (1c)` | mutmut, single child process | -->

## Methodology

### Measurement considerations

**Clean slate**: `mutants/` and `.irradiate/` are deleted before every run to avoid
result-caching effects.

**Warmup discarded**: One warmup run is performed before timed runs to warm OS disk
caches and JIT state. It is not included in the reported metrics.

**Timing method**: `/usr/bin/time -l` (macOS). This captures wall-clock time and peak
RSS without external dependencies. Per-process subprocess timing is not measured.

**Median reported**: The median of N timed runs is reported; min/max range is shown when
spread exceeds 50ms.

### What to compare

**Per-mutant time** is the primary metric. irradiate's pool mode keeps worker processes
alive across mutants, paying the startup cost once per worker rather than once per mutant.
irradiate `--isolate` spawns fresh subprocesses per mutant (comparable to many other
mutation testing tools).

**Mutation score** (killed / (killed + survived)) reflects operator coverage.

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
