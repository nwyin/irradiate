# irradiate — Phase 5 Roadmap

Status as of 2026-03-17. Specs 1–4 are complete: parse → mutate → stats → validate → test → report works end-to-end. This document tracks remaining work.

## Verification (all phases)

```bash
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
```

---

## 5.1 Missing Operators

Table-driven, quick to add:

| Operator | Type | Notes |
|----------|------|-------|
| String method swaps | Table | `.lower()↔.upper()`, `.lstrip()↔.rstrip()`, `.find()↔.rfind()` |
| Argument removal | Procedural | Remove each arg, replace with `None`. Need to inspect arg count, generate N variants. |
| Match case removal | Procedural | Drop each `case` branch from `match` statements. Python 3.10+. |

## 5.2 Pipeline Gaps

### Forced-fail validation

The trampoline already handles `active_mutant = "fail"` (raises `ProgrammaticFailException`). The pipeline's `validate_clean_run()` exists but there's no corresponding `validate_fail_run()` call. Wire it in after the clean run in `pipeline.rs`.

### pyproject.toml config

Add `toml` crate. Read `[tool.mutmut]` section: `paths_to_mutate`, `tests_dir`, `do_not_mutate`, `also_copy`, `debug`, `pytest_add_cli_args`. CLI flags override config values.

### Parallel mutation generation

`generate_mutants()` in `pipeline.rs` processes files sequentially. Add `rayon` to the hot path — swap the for-loop to `par_iter()` and collect results. Already listed as a dependency target in spec.md but not wired in.

### Content-addressable cache

Biggest remaining feature from design.md. Each mutation result keyed by:

```
cache_key = sha256(
    function_body_normalized,
    mutation_operator_id,
    mutation_index,
    test_set_hash,
    test_content_hash,
)
```

Store in `.irradiate/cache/`. Check before dispatching to worker pool. Skip on hit. Optional `--no-cache` flag and `--cache-url` for remote (S3/GCS).

### Worker pool hardening

- **Hot-accept after respawn**: respawned workers can't rejoin the accept loop (comment in `orchestrator.rs` line ~379).
- **Recycle every N mutants**: design.md says default 100, prevents pytest state leakage.
- **`--isolate` flag**: fallback to one-shot subprocess-per-mutant mode.
- **Memory monitoring**: respawn workers that exceed a configurable memory threshold.

### Skip rule gaps

- Type annotations not skipped (should never mutate hints).
- `len()` / `isinstance()` calls not skipped (mutations rarely produce useful signal).
- `# pragma: no mutate` collected but not enforced per-expression by line number — only per-function.

---

## 5.3 Mutation Testing irradiate Itself

Use irradiate to mutation-test its own Python harness files (`harness/__init__.py`, `worker.py`, `stats_plugin.py`). This is a small but non-trivial dogfooding exercise:

- Run `irradiate run` from the repo root with `--paths-to-mutate harness/`.
- Write dedicated pytest tests for the harness if coverage is weak.
- Track mutation score over time — add to CI as a quality gate once stable.

This also validates that irradiate handles its own harness code correctly (import hooks, socket setup, etc.).

## 5.4 CI

### GitHub Actions workflow

```yaml
# .github/workflows/ci.yml
name: CI
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo check && cargo clippy -- -D warnings
      - run: cargo test
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"
      - run: |
          cd tests/fixtures/simple_project
          python -m venv .venv
          .venv/bin/pip install pytest
      - run: bash tests/e2e.sh
```

Considerations:
- Cache `target/` and `.venv/` between runs.
- Run clippy with `-D warnings` so lint regressions fail the build.
- Integration tests need Python + pytest — install in CI.
- E2e tests need the built binary — run after `cargo build`.
- Add a separate job for `cargo test --release` to catch release-mode-only issues.

## 5.5 Pre-commit Checks

### `.pre-commit-config.yaml`

```yaml
repos:
  - repo: local
    hooks:
      - id: cargo-check
        name: cargo check
        entry: cargo check
        language: system
        pass_filenames: false
        types: [rust]
      - id: cargo-clippy
        name: cargo clippy
        entry: cargo clippy -- -D warnings
        language: system
        pass_filenames: false
        types: [rust]
      - id: cargo-fmt
        name: cargo fmt
        entry: cargo fmt -- --check
        language: system
        pass_filenames: false
        types: [rust]
      - id: cargo-test
        name: cargo test
        entry: cargo test
        language: system
        pass_filenames: false
        types: [rust]
        stages: [pre-push]
```

Notes:
- `cargo check` and `clippy` on pre-commit (fast).
- `cargo test` on pre-push (slower, includes integration tests).
- Consider `cargo fmt -- --check` to enforce formatting.
- Python harness linting: `uvx ruff check harness/ && uvx ruff format --check harness/`.

## 5.6 Static Analysis for Architecture Review

Generate artifacts that help human reviewers understand the codebase without reading every file.

### Call graph

```bash
# Rust call graph via cargo-callgraph or rust-analyzer exports
cargo install cargo-call-stack
cargo +nightly call-stack --bin irradiate > docs/artifacts/call-stack.dot
dot -Tsvg docs/artifacts/call-stack.dot -o docs/artifacts/call-stack.svg
```

Alternative: use `cargo-modules` for module-level dependency graph:

```bash
cargo install cargo-modules
cargo modules structure --no-fns > docs/artifacts/module-structure.txt
cargo modules dependencies --no-externs | dot -Tsvg -o docs/artifacts/module-deps.svg
```

### Import / dependency graph

```bash
cargo install cargo-depgraph
cargo depgraph | dot -Tsvg -o docs/artifacts/dep-graph.svg
```

### Control flow graphs

For specific hot functions (e.g., `collect_file_mutations`, `run_worker_pool`), use `cargo-show-asm` or MIR dumps:

```bash
cargo install cargo-show-asm
cargo asm irradiate::mutation::collect_file_mutations --rust
```

For the Python harness, use `py2cfg`:

```bash
uvx py2cfg harness/worker.py -o docs/artifacts/worker-cfg.svg
```

### Recommended artifacts to commit

```
docs/artifacts/
├── module-deps.svg       # Rust module dependency graph
├── call-stack.svg        # Binary call graph (top-level entry points)
├── dep-graph.svg         # Crate dependency graph
├── worker-cfg.svg        # Python worker control flow
└── README.md             # How to regenerate these
```

Regenerate on major refactors. Don't automate in CI — these are for human review during design discussions.

## 5.7 Vendored Repo Test Suite

Test irradiate against real-world Python projects to validate correctness and find edge cases.

### Candidate repos

| Repo | Why | Size |
|------|-----|------|
| `rich` | Complex string formatting, many operators, decorators | ~20k LOC |
| `requests` | HTTP library, well-tested, moderate size | ~5k LOC |
| `httpx` | Modern HTTP client, async code, type hints | ~10k LOC |
| `click` | CLI framework, lots of string manipulation | ~8k LOC |
| `pydantic-core` | Heavy use of classes, validators, edge cases | ~15k LOC |

### Setup

```bash
# Clone vendored test repos
mkdir -p tests/vendor_repos
cd tests/vendor_repos
git clone --depth 1 https://github.com/Textualize/rich.git
git clone --depth 1 https://github.com/psf/requests.git
git clone --depth 1 https://github.com/encode/httpx.git
```

Add `tests/vendor_repos/` to `.gitignore`. Write a test script:

```bash
# tests/vendor_test.sh
#!/usr/bin/env bash
set -euo pipefail
for repo in tests/vendor_repos/*/; do
    name=$(basename "$repo")
    echo "=== Testing $name ==="
    cd "$repo"
    uv venv && uv pip install -e ".[test]" 2>/dev/null || uv pip install -e ".[dev]" 2>/dev/null || true
    irradiate run --workers 4 --timeout-multiplier 5 2>&1 | tee "../../results_${name}.txt"
    cd -
done
```

### What to look for

- Parse failures (libcst can't handle the syntax).
- Trampoline wiring bugs (forced-fail validation catches these).
- Timeout tuning — real projects have slower tests.
- Mutant count sanity: compare against mutmut's count on the same repo.
- Crashes, hangs, socket errors under load.

## 5.8 Benchmark vs mutmut

### Methodology

Run both tools against the same repos. Measure:

1. **Wall-clock time** — full run, no cache, same machine.
2. **Mutant count** — how many mutants each tool generates (should be comparable).
3. **Mutation score** — killed / (killed + survived). Should be identical if operators match.
4. **Startup overhead** — time to first mutant result.
5. **Per-mutant overhead** — (total time - startup) / num_mutants.

### Script

```bash
# bench/compare.sh
#!/usr/bin/env bash
set -euo pipefail
REPO=${1:?usage: compare.sh <repo-path>}
RUNS=${2:-3}

echo "=== mutmut ==="
cd "$REPO"
rm -rf .mutmut-cache mutants/
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -l mutmut run 2>&1 | tee "mutmut_run_${i}.txt"
done

echo "=== irradiate ==="
rm -rf .irradiate/ mutants/
for i in $(seq 1 "$RUNS"); do
    /usr/bin/time -l irradiate run 2>&1 | tee "irradiate_run_${i}.txt"
done
```

### Expected results

- irradiate should be significantly faster on projects with many mutants (worker pool amortizes pytest startup).
- Mutant counts may differ (irradiate has fewer operators currently).
- Small projects (< 50 mutants): difference may be marginal — pytest startup dominates either way.

### Reporting

Produce a markdown table per repo:

```markdown
| Metric | mutmut | irradiate | Speedup |
|--------|--------|-----------|---------|
| Wall-clock (s) | 120.3 | 34.7 | 3.5x |
| Mutants generated | 847 | 812 | — |
| Mutation score | 68.2% | 67.1% | — |
| Per-mutant (ms) | 142 | 43 | 3.3x |
```

## 5.9 GitHub Pages Report

Host a summary page at `https://<user>.github.io/irradiate/` showing:

- Latest mutation testing results (killed/survived/timeout counts).
- Benchmark comparison table vs mutmut.
- Architecture diagrams (from §5.6 artifacts).
- Per-file mutation scores.

### Implementation

```
docs/site/
├── index.html          # Main report page
├── style.css           # Minimal styling
├── results.json        # Generated by CI or local script
└── bench.json          # Benchmark data
```

### Generator script

After each `irradiate run`, produce `results.json` from the `.meta` files:

```bash
# scripts/generate_report.sh
#!/usr/bin/env bash
set -euo pipefail
# Aggregate all .meta files into a single JSON report
python3 -c "
import json, glob, pathlib
meta_files = glob.glob('mutants/**/*.meta', recursive=True)
report = {'files': {}, 'summary': {'killed': 0, 'survived': 0, 'no_tests': 0, 'timeout': 0}}
for mf in meta_files:
    data = json.loads(pathlib.Path(mf).read_text())
    name = mf.replace('mutants/', '').replace('.meta', '')
    report['files'][name] = data
    for code in data.get('exit_code_by_key', {}).values():
        if code == 1: report['summary']['killed'] += 1
        elif code == 0: report['summary']['survived'] += 1
        elif code == 33: report['summary']['no_tests'] += 1
report['summary']['total'] = sum(report['summary'].values())
if report['summary']['total'] > 0:
    report['summary']['score'] = round(
        report['summary']['killed'] / (report['summary']['killed'] + report['summary']['survived']) * 100, 1
    )
print(json.dumps(report, indent=2))
" > docs/site/results.json
```

### GitHub Actions deployment

```yaml
# .github/workflows/pages.yml
name: Deploy report to GitHub Pages
on:
  workflow_run:
    workflows: ["CI"]
    types: [completed]
permissions:
  pages: write
  id-token: write
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/configure-pages@v4
      - uses: actions/upload-pages-artifact@v3
        with:
          path: docs/site
      - uses: actions/deploy-pages@v4
```

## 5.10 Design Review Notes

This section records architectural feedback after reading the design/spec docs and the current implementation, then verifying `cargo test` and `bash tests/e2e.sh`. The overall conclusion is positive: the repo has a real working vertical slice and the core design direction is still the right one. The main recommendation is to spend the next phase on correctness and worker semantics rather than feature expansion.

### What looks strong

- The Rust/Python split is good. Rust owns mutation planning, orchestration, reporting, and I/O; Python stays limited to the runtime pieces that must execute inside the test process.
- The trampoline approach is still the right performance-oriented design. Switching mutants through a global `active_mutant` is the key idea in the repo and is worth preserving.
- The codebase is small and understandable. `pipeline`, `mutation`, `trampoline`, `stats`, `orchestrator`, and the Python harness each have clear ownership.
- The vertical slice is real rather than aspirational. The end-to-end path works on the fixture project and the current test suite passes.

### Main critiques

#### The implementation does not yet fully realize the main worker-pool design

The docs describe workers that collect tests once and then execute selected pytest items directly without repeated `pytest.main()` calls. The current worker still calls `pytest.main(test_args)` for every mutant, even though it pre-collects test items. That means the pre-warmed pool is real, but the deepest startup savings and cleanest execution semantics are not yet implemented.

#### Mutation application is the biggest correctness risk

The mutation engine uses LibCST for discovery but applies mutations through text substitution. That was a pragmatic shortcut to get working software quickly, but it is now the main correctness limiter. Repeated identical tokens inside one function can map to the wrong source slice, and `# pragma: no mutate` is not yet enforced at expression granularity.

#### The execution/import model needs to be made explicit

Stats and clean validation run with `src/` on `PYTHONPATH`, while worker processes currently get the harness directory plus `mutants/`. That looks fragile for real projects where mutated files import untouched sibling modules. The repo should choose a stricter model:

- mirror the relevant source tree into `mutants/`, or
- stop relying on partial path shadowing and move to a controlled import-hook approach.

#### Worker lifecycle hardening is still pending

The orchestrator works for the happy path, but the harder operational cases are not done yet:

- timeouts are still coarse
- respawned workers cannot rejoin the pool
- the scheduler does not yet use collected test durations for ordering or timeout budgets
- long-lived pytest state leakage is acknowledged but not yet controlled through recycle/isolation strategy

These are the right next hardening targets before adding more surface area.

#### The docs should distinguish target architecture from current behavior more sharply

`design.md` is the target architecture. `roadmap.md` is the source of truth for current gaps and design decisions. (The original `spec.md` covered Specs 1-4, which are fully implemented and have been removed.)

### Recommended sequencing

If development continues from the current state, the suggested order is:

1. Tighten mutation correctness: move from substring-based application toward stable spans or structured rewriting.
2. Make worker execution semantics more honest: either truly reuse a persistent collected session, or recycle workers in bounded batches to contain pytest state leakage.
3. Unify the import/runtime model so stats, validation, and workers execute against the same module-resolution rules.
4. Use collected stats for actual scheduling and per-mutant timeout budgets.
5. Expand compatibility, config loading, cache, and operator coverage only after the foundation above is solid.

### Short version

The repo has a strong central idea and a good prototype architecture. The next step is not more features. The next step is making the existing design precise enough that mutation results, worker behavior, and imports are trustworthy on non-trivial Python codebases.

---

## 5.11 Design Decision: Python Import Model

### Current approach

All Python subprocess invocations (stats collection, validation, worker pool) set `PYTHONPATH` to include:

1. **Harness directory** — so `import irradiate_harness` resolves to our runtime package
2. **Mutants directory** — so `import mylib` resolves to the trampolined version in `mutants/`
3. **Source parent directory** — so unmutated sibling modules can be imported

This is a **path-shadowing** strategy: mutated files in `mutants/` shadow the originals because `mutants/` appears earlier on `PYTHONPATH` than the source directory.

### Tradeoffs

**Why this works for now:**
- Simple to implement — just a PYTHONPATH string
- No modifications to Python's import machinery
- Works for flat-layout projects where all source is under one directory

**Where it breaks:**
- **Partial mutation**: if we mutate `mylib/foo.py` but not `mylib/bar.py`, the `mutants/` directory contains only `foo.py`. When `foo.py` tries `from mylib import bar`, Python finds `mylib` in `mutants/` (because it has `__init__.py`) but `bar.py` isn't there. The import fails unless the original source directory is also on the path — but then `foo.py` might resolve from the wrong location depending on import order.
- **Namespace packages**: projects using implicit namespace packages (no `__init__.py`) break because Python can merge multiple directories into one namespace — the mutated and original directories might get merged unpredictably.
- **Editable installs**: projects installed with `pip install -e .` have their source resolved through `.pth` files and `pkg_resources` entry points, not plain `PYTHONPATH`. Our shadowing doesn't intercept these.
- **Relative imports**: `from . import sibling` resolves based on the package's `__path__`, which may point to the original source directory, not `mutants/`.

### Future alternatives

**Option A: Full source tree mirror.** Copy the entire source tree into `mutants/`, then overwrite only the mutated files. This eliminates the partial-mutation problem entirely — every import resolves within `mutants/`. Downside: disk I/O and potential for stale copies.

**Option B: Import hook.** Install a custom Python import hook (via `sys.meta_path`) that intercepts imports and redirects mutated modules to `mutants/` while letting everything else resolve normally. This is what mutmut's author originally wanted but abandoned due to import system fragility. It would be the cleanest solution but requires careful handling of `importlib` internals, cached bytecode (`.pyc`), and reload semantics.

**Option C: Symlink farm.** Create a directory with symlinks: mutated files point to `mutants/`, everything else points to original source. Single PYTHONPATH entry. Works well on Unix, less so on Windows. Fragile with tools that resolve symlinks.

**Recommendation:** Start with Option A (full mirror) when the current approach hits real-world breakage. It's the simplest correct solution and can be implemented incrementally by modifying `generate_mutants()` to copy unmutated files alongside mutated ones. Reserve Option B for a future optimization pass if the mirror approach proves too slow for large codebases.

---

## 5.12 Design Decision: Worker Execution Model

### Current approach

Workers call `pytest.main(["-x", "--no-header", "-q"] + test_ids)` for every mutant. This re-invokes pytest's full startup sequence — argument parsing, plugin loading, test collection, fixture resolution — then runs the selected tests and exits. The pre-collected test items from the `ItemCollector` plugin are used only for reporting available tests to the orchestrator, not for execution.

### Why this exists

This was a pragmatic shortcut to get the vertical slice working. `pytest.main()` is the only *public* API for running tests. It's well-documented, handles all edge cases (plugin lifecycle, fixture teardown, output capture, exit codes), and is guaranteed stable across pytest versions. Going deeper into pytest's internals trades stability for performance.

### The performance cost

On a typical project with 200ms pytest startup and 1000 mutants:
- **Current**: 1000 × 200ms = 200 seconds of pure startup overhead
- **Direct execution**: 1 × 200ms + 1000 × (test time only) ≈ 200ms + test time
- For fast tests (50ms each), this is the difference between 250s and 50s — a 5× speedup

This is the core value proposition of irradiate over mutmut. Without direct execution, the worker pool is just a process pool with warm Python interpreters, saving only the Python startup time (~50ms), not the pytest startup time (~200ms).

### What direct execution requires

The target API is `_pytest.runner.runtestprotocol(item, nextitem=None)`, which:
1. Calls setup hooks (fixture instantiation)
2. Runs the test function
3. Calls teardown hooks (fixture cleanup)
4. Returns a list of `TestReport` objects

Between mutant runs, the worker must reset:
- **Test outcomes**: clear any cached `TestReport` objects
- **Captured output**: reset the capture manager plugin
- **Fixture state**: session-scoped fixtures persist (by design), function-scoped fixtures are fresh per item
- **Plugin state**: some plugins accumulate state (warnings, coverage) that needs clearing

### Tradeoffs

**Gains:**
- 5-10× speedup on real projects (the whole point of irradiate)
- Lower memory churn (no repeated pytest Session objects)
- More accurate timing (measures test execution, not pytest overhead)

**Risks:**
- **Pytest internal API instability**: `_pytest.runner.runtestprotocol` is private API. It could change between pytest versions. Mitigation: version-check at startup, fall back to `pytest.main()` on unrecognized versions.
- **State leakage**: session-scoped fixtures, module-level variables, and global state survive between runs. A test that sets `os.environ["API_KEY"] = "test"` without cleanup will affect subsequent runs. Mitigation: worker recycling (respawn every N mutants).
- **Plugin compatibility**: some pytest plugins assume one session = one run. Plugins that accumulate state (pytest-cov, pytest-xdist) may produce incorrect results. Mitigation: document incompatible plugins, offer `--isolate` fallback.
- **Fixture teardown ordering**: running items out of collection order may trigger fixtures in unexpected sequences. Mitigation: run items in their original collection order within each mutant.

### Phased approach

1. **Phase 1 (current)**: `pytest.main()` per mutant. Correct but slow. Validates the pool architecture.
2. **Phase 2 (next)**: `runtestprotocol()` on pre-collected items. The main performance win. Add version check + fallback.
3. **Phase 3 (later)**: Worker recycling. Respawn every N mutants to bound state leakage. Configurable via `--worker-recycle-after`.
4. **Phase 4 (optional)**: `--isolate` flag. Fresh subprocess per mutant for projects that can't tolerate any state sharing. Deliberately slow but maximally correct.

### What to watch for

When we ship Phase 2, the first real-world test suite that breaks will likely be due to one of:
- A session-scoped fixture that caches state incorrectly
- A plugin that writes to a shared file (coverage data, timing reports)
- A test that monkeypatches a module attribute without cleanup

The `--isolate` flag exists as a debugging escape hatch for these cases. If a user reports "irradiate gives different results than pytest", the first diagnostic step is `irradiate run --isolate` — if that matches pytest, the bug is state leakage in pool mode.

---

## Priority Order

| # | Item | Effort | Impact |
|---|------|--------|--------|
| 1 | Forced-fail validation (§5.2) | S | Correctness — catches broken trampolines |
| 2 | String method swap operators (§5.1) | S | More mutants, quick table addition |
| 3 | Pre-commit checks (§5.5) | S | Prevents regressions immediately |
| 4 | CI (§5.4) | S | Automated verification on every push |
| 5 | pyproject.toml config (§5.2) | M | UX — users expect config file support |
| 6 | Parallel mutation gen (§5.2) | S | Perf win, trivial with rayon |
| 7 | Vendored repo tests (§5.7) | M | Finds real-world edge cases |
| 8 | Benchmark vs mutmut (§5.8) | M | Validates the "why" of the project |
| 9 | Static analysis artifacts (§5.6) | S | Aids contributors and reviewers |
| 10 | Mutation testing own harness (§5.3) | M | Dogfooding, harness quality |
| 11 | GitHub Pages report (§5.9) | M | Visibility, shareable results |
| 12 | Content-addressable cache (§5.2) | L | Big perf win on incremental runs |
| 13 | Argument removal / match case (§5.1) | M | More complete operator coverage |
| 14 | Worker pool hardening (§5.2) | L | Robustness at scale |
