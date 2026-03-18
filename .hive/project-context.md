# Project Context — irradiate

## Overview
Mutation testing tool for Python, written in Rust — spiritual successor to mutmut. Parses Python source with libcst, generates mutant variants via trampoline code injection, then runs pytest workers over Unix sockets to classify each mutant as killed/survived.

## Architecture
- **mutation.rs** — Parse Python via libcst-native, walk the CST to collect mutation points (binop swap, compop swap, boolop swap, name/number/string/lambda mutations, method swaps, assignment mutations). Each `Mutation` carries byte-span offsets within the function source.
- **trampoline.rs** — Name-mangle functions (mutmut convention: `x_func` / `xǁClassǁmethod`), generate orig + variant defs + lookup dict + trampoline wrapper that dispatches via `irradiate_harness.active_mutant`.
- **codegen.rs** — File-level codegen: strip original function defs, prepend trampoline runtime, append all trampoline arrangements. Produces `MutatedFile` with source + mutant name list.
- **pipeline.rs** — Full pipeline orchestration: discover .py files → mutate (rayon parallel) → write to `mutants/` dir → optional stats collection → optional forced-fail validation → dispatch to worker pool → write `.meta` results → print report. Also implements `results` and `show` subcommands.
- **orchestrator.rs** — Tokio-based worker pool: spawn N Python processes, accept Unix socket connections, dispatch `Run` messages, collect `Result`/`Error` messages, recycle workers after N mutants, respawn crashed workers.
- **harness/** — Embedded Python package (`irradiate_harness`): `__init__.py` (active_mutant global, hit recording), `worker.py` (pytest plugin that intercepts `pytest_runtestloop` for IPC-driven test execution), `stats_plugin.py` (coverage collection via `--irradiate-stats`).

Data flow: CLI → `pipeline::run` → `codegen::mutate_file` (parallel via rayon) → write mutated sources to `mutants/` → `stats::collect_stats` (single pytest run in stats mode) → build `WorkItem` list with targeted test IDs → `orchestrator::run_worker_pool` (tokio, Unix sockets) → Python workers run pytest items directly → results written to `.meta` JSON files.

## Key Files
- `src/main.rs` — CLI entrypoint (clap), subcommands: `run`, `results`, `show`
- `src/pipeline.rs` — Full pipeline: mutate → stats → validate → test → report (~600 lines, the big one)
- `src/mutation.rs` — Mutation engine: CST walking, operator tables, byte-span tracking (~1100 lines with tests)
- `src/trampoline.rs` — Trampoline codegen: name mangling, wrapper generation, runtime dispatch code
- `src/codegen.rs` — File-level mutation: strip originals, inject trampolines
- `src/orchestrator.rs` — Tokio worker pool: spawn, dispatch, recycle, respawn
- `src/protocol.rs` — IPC message types: `OrchestratorMessage`, `WorkerMessage`, `MutantResult`, `MutantStatus`
- `src/harness.rs` — Extract embedded Python harness to `.irradiate/harness/`
- `src/config.rs` — Load `[tool.mutmut]` from pyproject.toml
- `src/stats.rs` — Stats collection: run pytest in stats mode, load/query results
- `harness/__init__.py` — Python runtime: `active_mutant` global, `ProgrammaticFailException`, hit recording
- `harness/worker.py` — Pytest worker: Unix socket IPC, direct item execution via `runtestprotocol`
- `harness/stats_plugin.py` — Pytest plugin for per-test function coverage
- `tests/e2e.sh` — End-to-end test: build, run on fixture, verify killed/survived counts, test `--isolate` flag

## Build & Test
- **Language**: Rust 2021 edition + Python 3.12 (harness)
- **Package manager**: Cargo (Rust), uv (Python fixtures)
- **Build**: `cargo build`
- **Test**: `cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh`
- **Lint**: `cargo clippy -- -D warnings`
- **Format**: `cargo fmt` (Rust), `uvx ruff format` with line-length=144 (Python)
- **Type check**: N/A (no mypy/pyright configured for harness)
- **Pre-commit**: N/A (manual: must run check + clippy + test before every commit per CLAUDE.md)
- **Quirks**: E2E tests require `cd tests/fixtures && uv venv && uv pip install pytest`. The e2e.sh creates venvs automatically if missing. Python harness files are embedded via `include_str!` at compile time — edits to `harness/*.py` require `cargo build` to take effect. Unix socket paths use `/tmp` to avoid macOS 104-byte path limit.

## Conventions
- Mutation naming follows mutmut: `x_func` (top-level), `xǁClassǁmethod` (class methods), `__mutmut_orig`/`__mutmut_N` suffixes
- Mutant keys are module-qualified: `module.x_func__mutmut_1`
- Results stored as JSON `.meta` files in `mutants/<module_path>.py.meta`
- Tests are inline `#[cfg(test)] mod tests` in each Rust source file — no separate test directory for Rust
- Python harness uses `# pragma: no mutate` to skip self-mutation of critical functions
- Error handling via `anyhow::Result` throughout; `tracing` for structured logging
- IPC protocol is newline-delimited JSON over Unix domain sockets
- snake_case everywhere (Rust convention); serde `rename_all = "snake_case"` for JSON
- Decorated Python functions are skipped (not mutated)
- `NEVER_MUTATE_FUNCTIONS`: `__getattribute__`, `__setattr__`, `__new__`

## Dependencies & Integration
- **libcst (1.8.6)** — Rust port of Python's libcst; used for parsing Python source into CST for mutation point discovery. No-default-features (native parser only).
- **tokio** — Async runtime for worker pool orchestration, Unix socket communication
- **rayon** — Parallel mutation generation across source files
- **clap 4** — CLI argument parsing with derive macros
- **serde/serde_json** — JSON serialization for IPC protocol and .meta result files
- **toml** — Parse pyproject.toml for `[tool.mutmut]` config
- **pytest** — Test runner (invoked as subprocess); worker.py uses `_pytest.runner.runtestprotocol` for direct item execution
- **proptest** (dev) — Property-based testing for mutation engine

## Gotchas
- Byte-span offsets in `Mutation` are relative to the function source string, not the full file. The cursor advances monotonically to handle duplicate tokens (e.g., two `+` operators).
- `codegen.rs` strips original functions by indent level — if a function has inner functions at the same indent, they may be incorrectly stripped.
- Worker recycling (`worker_recycle_after`) is critical for long runs: pytest accumulates state that eventually causes failures if workers aren't respawned.
- The `--isolate` flag runs each mutant in a fresh subprocess (slower but avoids all state leakage); e2e tests verify it produces identical results to worker pool mode.
- Config reads `[tool.mutmut]` (not `[tool.irradiate]`) for backward compatibility with mutmut.
- `PYTHONPATH` construction in `pipeline.rs` must include harness dir, mutants dir, and project source parent for correct import resolution.
- Stats mode (`active_mutant = "stats"`) records which functions each test calls; this is used to build targeted test lists per mutant, dramatically reducing run time.
- Forced-fail validation runs a single mutant with `active_mutant = "fail"` to verify the trampoline wiring works before the full run.
