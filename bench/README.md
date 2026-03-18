# irradiate vs mutmut benchmarks

Scripts to compare irradiate and mutmut on shared Python test targets.

## Setup

Run once from the project root:

```bash
bash bench/setup.sh
```

This will:
1. Build `target/release/irradiate` (release mode)
2. Create `bench/.venv` with mutmut and pytest installed
3. Create `tests/fixtures/simple_project/.venv`
4. Create `vendor/mutmut/e2e_projects/my_lib/.venv`

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
| `mutmut (Nc)` | mutmut, all CPU cores (`--max-children N`) |
| `mutmut (1c)` | mutmut, single child process |

## Methodology

### Fairness considerations

**Clean slate**: `mutants/` and `.irradiate/` are deleted before every run.
mutmut skips mutants with existing results, so stale state would skew timings.

**Same Python interpreter**: Both tools use the same venv Python for running tests.
irradiate passes `--python $PYTHON`; mutmut uses the Python in `$PROJECT_DIR/.venv`
(the working directory's venv).

**Matched parallelism**: `--workers N` (irradiate) matches `--max-children N` (mutmut).
Default `N` is `$(sysctl -n hw.ncpu)` (all logical cores).

**Warmup discarded**: One warmup run is performed before timed runs to warm OS disk
caches and JIT state. It is not included in the reported metrics.

**Timing method**: `/usr/bin/time -l` (macOS). This captures wall-clock time and peak
RSS without external dependencies. Per-process subprocess timing is not measured.

**Median reported**: The median of N timed runs is reported; min/max range is shown when
spread exceeds 50ms.

### What to compare

**Per-mutant time** is the fairest comparison metric. Mutant counts differ between tools
because irradiate and mutmut implement different mutation operators. A tool that generates
more mutants will naturally take longer in absolute terms.

**Mutation score** (killed / (killed + survived)) should be similar if both tools have
reasonable operator coverage, but may differ for the same reason.

### Fork model difference

mutmut uses `os.fork()` from a warmed Python process (copy-on-write). irradiate `--isolate`
spawns fresh `python -m pytest` subprocesses. irradiate's pool mode is the real
differentiator: it keeps worker processes alive across mutants, paying the startup cost
once per worker rather than once per mutant.

`irradiate --isolate` is the most apples-to-apples comparison with mutmut's single-child
mode, since both spawn a fresh process per mutant.

## Re-running summarize.py

If you want to regenerate the table from existing results without re-running the tools:

```bash
bench/.venv/bin/python bench/summarize.py \
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
