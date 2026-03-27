# irradiate benchmarks

Benchmark suite comparing irradiate against mutmut 3.5.0 on real-world Python projects.

## Quick start

```bash
# One-time setup (builds release binary, installs mutmut, sets up project venvs)
bash bench/setup.sh

# Run all benchmarks (6 targets, 3 runs each — takes a while)
bash bench/run_all.sh

# Quick smoke test (1 run, 2 targets)
bash bench/run_all.sh --runs 1 --targets "markupsafe itsdangerous"
```

## Targets

All targets are well-known PyPI packages with real test suites.

| Target | Package | Layout | Description |
|---|---|---|---|
| `markupsafe` | pallets/markupsafe | src | HTML escaping library, tiny |
| `itsdangerous` | pallets/itsdangerous | src | Signed data helpers |
| `toolz` | pytoolz/toolz | flat (tests inside pkg) | Functional utilities |
| `marshmallow` | marshmallow-code/marshmallow | src | Object serialization |
| `more-itertools` | more-itertools/more-itertools | flat | Extended itertools |
| `click` | pallets/click | src | CLI framework, largest target |

Additional targets exist for development use (`simple_project`, `my_lib`, `synth`, `tinygrad`).

## Configurations tested

| Config | Tool | Workers | Description |
|---|---|---|---|
| `irradiate pool (Nw)` | irradiate | all CPUs | Persistent worker pool, full parallelism |
| `irradiate pool (1w)` | irradiate | 1 | Single worker, shows per-mutant overhead |
| `irradiate isolate` | irradiate | sequential | Fresh subprocess per mutant |
| `mutmut (Nc)` | mutmut 3.5.0 | all CPUs | mutmut with full parallelism |
| `mutmut 3.5.0` | mutmut 3.5.0 | 1 | Single-child baseline |

## What the benchmark measures

**Speed**: Wall-clock time via `/usr/bin/time -l` (macOS) or `-v` (Linux). Each config gets
1 warmup run (discarded) plus N timed runs. Median is reported.

**Correctness**: Mutant counts, killed/survived, and mutation score for each tool.
Operator breakdown (per-operator kill rates) for irradiate via Stryker JSON reports.

**Key metric**: Speedup = mutmut wall time / irradiate wall time (geometric mean across targets).

## Methodology

**Clean slate**: `mutants/`, `.irradiate/`, and `.mutmut-cache` are deleted before every run.

**Warmup discarded**: One warmup run warms OS disk caches and JIT state. Not included in results.

**Median reported**: The median of N timed runs. Min/max range shown when spread > 50ms.

**Mutant counts differ**: Operator coverage is not identical between tools. This is expected.
irradiate has ~38 operator categories vs mutmut's ~20. ms/mutant normalizes for this.

## Output

Single-target results go to `bench/results/<timestamp>/<target>/`:
- `summary.md` — markdown table
- `raw_data.json` — structured data
- `*_report.json` — Stryker JSON reports (irradiate configs only)

Aggregate results (from `run_all.sh`) go to `bench/results/<timestamp>/`:
- `aggregate.md` — cross-target speed + correctness comparison with geometric mean speedups
- `aggregate.json` — machine-readable aggregate data

## Running a single target

```bash
bash bench/compare.sh markupsafe --runs 3
```

## Adding a new target

1. Create `bench/targets/<name>.sh` exporting `PROJECT_DIR`, `PATHS_TO_MUTATE`, `TESTS_DIR`, and `PYTHON`.
2. Add the project's clone to `scripts/bootstrap-vendors.sh`.
3. Add venv setup + mutmut venv install to `bench/setup.sh`.
4. Run: `bash bench/compare.sh <name>`

## Environment overrides

| Variable | Default | Description |
|---|---|---|
| `BENCH_RUNS` | 3 | Number of timed runs per config |
| `BENCH_TARGETS` | all 6 | Space-separated target names for `run_all.sh` |
| `BENCH_MUTMUT` | (unset) | Set to `1` to force mutmut runs on CI |
| `BENCH_ISOLATE` | (unset) | Set to `1` to force irradiate isolate on CI |
| `BENCH_TIMESTAMP` | (auto) | Shared timestamp for grouping results |
| `MUTMUT_VERSION` | 3.5.0 | mutmut version to install |
