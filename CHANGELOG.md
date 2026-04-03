# Changelog

All notable changes to irradiate are documented here.

## 0.4.1 — 2026-04-04

### Fixes

- **Python interpreter auto-detection** — irradiate now finds the correct Python interpreter when installed into a virtualenv via `uv pip install` or `pip install`. Previously it defaulted to bare `python3` on PATH, which could resolve to the system Python (without pytest). Resolution order: sibling of the irradiate binary, then `$VIRTUAL_ENV/bin/python3`, then PATH. The `--python` flag still overrides all auto-detection.

## 0.4.0 — 2026-04-03

### Features

- **Decorator support** — functions with any decorator (`@lru_cache`, `@app.route`, `@login_required`, custom decorators) are now mutated via a source-patching fallback. Previously these were skipped entirely. Use `--no-source-patch` to opt out.
- **`decorator_removal` operator** — each decorator on a function is a separate mutant where that decorator line is removed. Tests whether decorators like `@cache`, `@login_required`, or `@retry` are actually tested.
- **Type-check filter** (`--type-checker mypy|pyright|ty`) — runs a type checker against mutated code after generation. Mutants that introduce type errors are marked as killed and skipped from test execution. On well-typed codebases this eliminates ~35% of test runs.
- **Remote cache sync hooks** (`cache_pre_sync` / `cache_post_sync`) — shell commands that run before/after mutation testing, enabling shared caches across CI runs via S3, GCS, rsync, or any backend.
- **Cache garbage collection** (`irradiate cache gc`) — prune old or oversized cache entries with `--max-age`, `--max-size`, and `--dry-run`. Defaults configurable in pyproject.toml.
- **`--ignore` CLI flag** — shorthand for `do_not_mutate` config. Glob patterns to exclude files from mutation, can be repeated.
- **Auto-generated CLI docs** — `scripts/gen-cli-docs.sh` generates `docs/reference/cli.md` from clap definitions, keeping docs in sync with code.

### Performance

- **Parallel worker accept handshakes** — worker ready-message reads now happen in background tokio tasks instead of blocking the accept loop sequentially. Reduces startup latency at high worker counts.
- **Eager dispatch** — work is dispatched to idle workers immediately after each event, reducing gaps between mutant executions.

### Fixes

- **Type checker path resolution** — fixed double `mutants/` prefix in error-to-mutant mapping that caused zero matches when running `--type-checker`.
- **`cache_pre_sync` hook timing** — moved to fire before stats collection (which reads the cache), not after.

## 0.3.0 — 2026-03-26

### Features

- **Trampoline-free stats collection** — `sys.monitoring` (Python 3.12+) or `sys.settrace` (3.10-3.11) replaces the trampoline-based stats plugin. Tests run against original unmodified source during coverage collection, eliminating import hook overhead and trampoline artifacts. Faster, more reliable, and compatible with more projects.
- **Regex mutation operators** — 11 new operators targeting regex patterns: anchor removal, character class negation, quantifier boundary changes, group simplification, alternation removal, escape removal, lookahead removal, dot-to-literal, and more.
- **`constant_replacement` operator** — replaces numeric constants with 0 and their negation (`n → 0`, `n → -n`).
- **Per-operator kill rates** — terminal summary now shows mutation score broken down by operator category, making it easy to see which operators produce the most survivors.
- **`--no-cache` flag** — bypass the result cache for a clean run.
- **`--stats-timeout` flag** — configurable timeout for stats collection (default 300s), useful for large test suites.
- **Grouped survivor output** — survived mutants are grouped by operator in terminal output for easier triage.

### Performance

- **Removed count-based worker recycling** — under the fork-per-mutant model, workers never execute test code, so state leakage is impossible. Removing recycling eliminates ~500ms respawn overhead that previously triggered every 20 mutants when session-scoped fixtures were detected. On real-world projects: throughput on slow mutants improved from ~6.5/s to ~15-16/s.
- **Prescan import hook** — reduces per-import overhead by pre-scanning which modules have mutated files.

### Compatibility

Ran irradiate against 87 open-source Python projects via a new compatibility sweep. Fixed 20+ issues discovered:

- Preserve `@property` setters when getter is trampolined.
- Skip regex mutations on f-strings to avoid SyntaxError.
- Handle ternary expressions in `with` clauses and walrus operator contexts.
- String-aware parameter splitting for functions with default string arguments containing commas/parens.
- Copy non-Python data files (`.txt`, `.json`, `.grammar`) to mutants directory for packages that locate data via `__file__`.
- Filter comment nodes from ternary children to prevent garbled mutations.
- Drain stderr in background thread to prevent pipe-buffer deadlock on large pytest output.
- Preserve lambda defaults during annotation stripping.
- Tab-indent support for projects not using spaces.
- Strip annotations from wrapper functions, skip `_getframe` and `__init_subclass__`.
- Surface pytest collection errors with actionable messages instead of silent hangs.

## 0.2.0 — 2026-03-23

### Features

- **`--sample N` flag** — randomly sample a subset of mutants for fast CI feedback. Values 0.0-1.0 are fractions (e.g. `--sample 0.1` = 10%), values > 1 are absolute counts. Operator-stratified with deterministic seeding (`--sample-seed`). Academic basis: Wong et al. 1995 — 5% random sampling gives 99% R² correlation with the full mutation score.
- **Multi-file `--paths-to-mutate`** — accepts multiple paths (`--paths-to-mutate src/a.py --paths-to-mutate src/b.py`).
- **Survivor context** — survived mutant output now shows file path, line number, and a description of what changed.
- **Skip uncovered functions** — functions with zero test coverage are marked `NoTests` immediately during scheduling instead of running the full test suite against them. With `--covered-only`, they're excluded from results entirely.
- **GitHub Actions composite action** — `nwyin/irradiate@v0` for drop-in CI integration with inline annotations, step summary, and score outputs.

### Performance

- **Pre-spawn workers during stats** — workers boot (Python + pytest collection, ~480ms) in parallel with stats collection (~3-4s) instead of sequentially. Worker utilization improved from 63% to 93%.
- **Stats caching** — fingerprint source + test files with SHA256. When nothing changed, skip the 4+ second pytest stats run entirely. Non-pool overhead dropped from 36% to 0.6% on repeat runs.
- **Native RSS monitoring** — `proc_pidinfo` (macOS) / `/proc/statm` (Linux) instead of spawning `ps` subprocess every 2 seconds per worker.
- **Smart worker recycling** — skip count-based recycling when the work queue is nearly drained, avoiding 500ms startup overhead for replacement workers that would only process 1-2 mutants.
- **Release profile** — LTO + single codegen unit for better inlining in release builds.
- **Progress bar overhaul** — multi-line display with per-worker activity, throttled to 100ms renders.
- **Full pipeline tracing** — trace.json now covers all pipeline phases (generation, stats, scheduling, worker pool, results) with Perfetto-compatible thread labels. Added `scripts/analyze_trace.py` for trace analysis.

### Mutation pruning

- **Kaminski ROR** — relational operator replacement uses the mathematically sufficient 3-mutant set per operator instead of naive pairwise swaps. 57% reduction on relational operators.
- **Arid node filtering** — skip mutations inside display-only methods (`__repr__`, `__str__`), trampoline-incompatible methods (`__getattribute__`, `__new__`), and logging/warning calls.
- **Equivalent mutant suppression** — pattern-matching rules for `len(x) > 0 → len(x) >= 0` (always equivalent) and string `a + b → a - b` (always TypeError).
- **String operator dedup** — `string_mutation` ("XXhelloXX") removed, only `string_emptying` ("") retained. If code doesn't catch empty string, it won't catch the wrapped version either.
- **Arg removal dedup** — arity-changing argument removal removed, only None-replacement retained. Removal usually just crashes with TypeError.

### Fixes

- **Import hook no longer hijacks partially mutated packages** — when only a subset of files in a package are mutated (e.g. `--paths-to-mutate httpx/_content.py`), the import hook previously created a namespace package that shadowed the real package, breaking all other imports. Now the hook only intercepts modules with actual mutated files, and preserves the original package's search locations.
- **Better error messages** — worker crashes now surface stderr from the failed process. Socket, spawn, timeout, and "no mutations found" errors include actionable context (paths, durations, common causes).

### Internal

- **Removed libcst dependency** — all parsing and validity checks now use tree-sitter. Simpler dependency tree, faster test builds.
- **Release workflow updates floating tags** — pushing `v0.2.0` automatically updates `v0` and `v0.2` tags for semver-compatible action references.

## 0.1.1 — 2026-03-21

Bugfix release targeting real-world project compatibility.

### Fixes

- **Import hook uses `spec_from_file_location`** so `__file__` works correctly in trampolined modules. Previously modules loaded via the import hook had no `__file__` attribute, breaking code that introspects its own path.
- **Single-file `paths_to_mutate` preserves package structure** — mutating a single file like `src/mylib/core.py` now correctly preserves the `mylib/` package hierarchy in the mutants directory.
- **Accept array-typed `paths_to_mutate` and `tests_dir`** in pyproject.toml. Previously only string values worked; TOML arrays caused a parse error.
- **Codegen body-stripping handles multi-line triple-quoted strings** — functions containing triple-quoted strings that span many lines no longer produce invalid Python in the trampoline.
- **Set `has_location=True` on import hook `ModuleSpec`** so pytest and other tools can find the source file for trampolined modules.

## 0.1.0 — 2026-03-19

Initial release.

### Highlights

- **27 mutation operator categories** — arithmetic, comparison, boolean, string, assignment, return value, argument removal, default arguments, exception broadening, decorator removal, match/case branch removal, ternary swap, loop mutation, statement deletion, and more.
- **Warm-session worker pool** — fork-per-mutant execution within a pre-warmed pytest session. 30-60 mutants/sec on typical projects vs 1-2/sec for traditional tools.
- **Coverage-based test selection** — per-function test mapping via the stats plugin. Only tests that cover a mutant's function are run, not the full suite.
- **Content-addressable cache** — SHA256-based caching of mutation results. Unchanged source + tests = instant results on re-run.
- **Incremental mode (`--diff`)** — only mutate functions changed since a git ref (e.g. `--diff main`).
- **Reports** — Stryker mutation-testing-report-schema v2 JSON, self-contained HTML report, and GitHub Actions annotations with step summary.
- **`--fail-under`** — exit code 1 when mutation score drops below threshold, for CI gates.
- **`--isolate`** — subprocess-per-mutant mode for perfect isolation when warm-session causes false negatives.
- **`--verify-survivors`** — re-test survived mutants in isolate mode to detect warm-session false negatives.
- **Session fixture detection** — auto-tunes worker recycling interval when session-scoped fixtures are detected.
- **Descriptor-aware trampolining** — correct dispatch for `@property`, `@classmethod`, `@staticmethod`.
- **Tree-sitter parser** — fast, reliable Python parsing without a Python runtime dependency.
- **pyproject.toml config** — `[tool.irradiate]` section with backward compatibility for `[tool.mutmut]` configs.
- **Multi-platform wheels** — Linux x86_64/aarch64, macOS x86_64/arm64 via maturin.
