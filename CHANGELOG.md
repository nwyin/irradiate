# Changelog

All notable changes to irradiate are documented here.

## Unreleased

### Features

- **`--sample N` flag** — randomly sample a subset of mutants for fast CI feedback. Values 0.0-1.0 are fractions (e.g. `--sample 0.1` = 10%), values > 1 are absolute counts. Operator-stratified with deterministic seeding (`--sample-seed`). Academic basis: Wong et al. 1995 — 5% random sampling gives 99% R² correlation with the full mutation score.
- **Multi-file `--paths-to-mutate`** — accepts multiple paths (`--paths-to-mutate src/a.py --paths-to-mutate src/b.py`).
- **Survivor context** — survived mutant output now shows file path, line number, and a description of what changed.
- **Skip uncovered functions** — functions with zero test coverage are marked `NoTests` immediately during scheduling instead of running the full test suite against them. With `--covered-only`, they're excluded from results entirely.

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

### Internal

- **Removed libcst dependency** — all parsing and validity checks now use tree-sitter. Simpler dependency tree, faster test builds.

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
