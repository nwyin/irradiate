# irradiate — Implementation Spec

Mutation testing for Python, written in Rust. Full end-to-end pipeline: parse Python → generate trampolined mutants → orchestrate pytest worker pool → report results.

## Verification (all phases)

```bash
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
```

## Constraints

- Rust stack: tokio (async orchestrator), clap (CLI), rayon (parallel mutation gen)
- Parser: `libcst` crate from crates.io (Meta). If it can't handle mutation codegen, fall back to `tree-sitter-python` + manual codegen.
- Python harness (worker.py, trampoline.py, stats_plugin.py) is embedded in the Rust binary, extracted at runtime to `.irradiate/harness/`.
- No TUI (defer). No cache (defer). No `--test-command` fallback (defer). Pytest worker pool only.
- Full operator catalog from design doc (table-driven + procedural).
- Loose mutmut compatibility — similar naming convention but our own output format is fine.
- E2e tests run against mutmut's `e2e_projects/` (cloned into `tests/e2e_projects/`).

---

## Spec 1: Scaffold + libcst Spike

### Requirements

- Initialize Cargo workspace with binary crate `irradiate` and library crate `irradiate-core`.
- Add dependencies: `libcst`, `clap`, `tokio`, `rayon`, `serde`, `serde_json`.
- Write a spike program (`examples/spike_libcst.rs`) that:
  1. Parses a simple Python file with one function using `libcst`.
  2. Walks the CST to find `BinaryOp::Add` nodes.
  3. Replaces `+` with `-` in a copy of the tree.
  4. Codegens the modified tree back to valid Python source.
- If `libcst` cannot do step 3 or 4 (immutable tree, no codegen API), document the gap and scaffold a `tree-sitter-python` fallback module behind a feature flag.
- Create `tests/fixtures/simple.py` — a minimal Python file with arithmetic functions and tests.
- Create `tests/e2e.sh` — a skeleton e2e script that just runs `cargo build --release` and exits 0.
- Clone mutmut's `e2e_projects/` into `tests/e2e_projects/` (add to `.gitignore` if large; use a git submodule or download script).

### Success Criteria

- `cargo check && cargo clippy -- -D warnings && cargo test` passes.
- `cargo run --example spike_libcst` successfully parses, mutates, and prints modified Python source.
- If libcst doesn't work: a documented decision in `docs/parser-decision.md` with the fallback plan.

### Ralph Command

```
/ralph-loop:ralph-loop "Read docs/spec.md and implement Spec 1: Scaffold + libcst Spike" --max-iterations 30 --completion-promise "cargo check && cargo clippy -- -D warnings && cargo test passes, and spike example runs"
```

---

## Spec 2: Mutation Engine

**Prerequisites:** Spec 1 (parser confirmed working)

### Requirements

- Module `irradiate-core::mutation` with:
  - `MutationOperator` trait: `fn id(&self) -> &str` + `fn mutate(&self, node, ctx) -> Vec<Mutation>`.
  - Table-driven operators as static data:
    - `BINARY_OP_SWAPS` — arithmetic (`+↔-`, `*↔/`, `//`, `%`, `**`), comparison (`<↔<=`, `>↔>=`, `==↔!=`), bitwise (`&↔|↔^`, `<<↔>>`), logical (`and↔or`).
    - `KEYWORD_SWAPS` — `is↔is not`, `in↔not in`, `break→return`, `continue→break`.
    - `BOOL_SWAPS` — `True↔False`, `deepcopy↔copy`.
    - `METHOD_SWAPS` — `lower↔upper`, `lstrip↔rstrip`, `find↔rfind`.
    - Unary removal — `not x → x`, `~x → x`.
  - A generic CST walker that applies all table-driven operators by matching node types.
  - Procedural operators: `NumberMutation` (n → n+1), `StringMutation` (case swap), `LambdaMutation` (body → None), `ArgumentRemoval`, `AssignmentMutation`.
- Module `irradiate-core::trampoline` with:
  - Given a function and its mutations, generate: renamed original (`x_func__mutmut_orig`), N variants (`x_func__mutmut_1..N`), lookup dict, trampoline wrapper.
  - Output is valid Python source as a `String`.
- Module `irradiate-core::codegen` with:
  - Given a Python source file, produce the fully mutated version (all functions trampolined, `import irradiate_harness` prepended).
  - Parallel file processing with rayon.
  - Write output to `mutants/` directory.
  - Write `.meta` JSON stub per file (list of mutant names, no results yet).
- Skip rules: `# pragma: no mutate`, dunder methods (`__getattribute__`, `__setattr__`, `__new__`), type annotations, decorator expressions, docstrings.
- Unit tests for each operator (input Python snippet → expected mutated snippets).
- Unit tests for trampoline generation (input function → expected trampolined output).

### Success Criteria

- `cargo test` passes with ≥1 test per operator and ≥1 trampoline generation test.
- Running mutation generation on `tests/fixtures/simple.py` produces correct trampolined output in `mutants/`.

### Ralph Command

```
/ralph-loop:ralph-loop "Read docs/spec.md and implement Spec 2: Mutation Engine" --max-iterations 30 --completion-promise "cargo test passes with operator and trampoline tests"
```

---

## Spec 3: Python Harness + Worker Pool

**Prerequisites:** Spec 2 (mutation engine produces mutant files)

### Requirements

- Embedded Python harness files (compiled into binary via `include_str!`):
  - `harness/trampoline.py` — holds `active_mutant` global, `record_hit()` for stats mode, `ProgrammaticFailException`.
  - `harness/worker.py` — connects to unix socket, receives JSON commands, sets `active_mutant`, runs pytest items directly (not `pytest.main()`), reports results. ~100 lines.
  - `harness/stats_plugin.py` — pytest plugin that records which tests call which trampolined functions when `active_mutant == "stats"`.
- At startup, extract harness files to `.irradiate/harness/` (overwrite each run).
- Module `irradiate-core::orchestrator` with:
  - Spawn N worker processes (N = num CPUs) that run `worker.py`.
  - Each worker connects to a shared unix domain socket.
  - IPC: newline-delimited JSON as specified in design doc (`warmup`, `run`, `shutdown` messages; `ready`, `result`, `error` responses).
  - Async event loop (tokio) manages all worker connections concurrently.
  - Work queue: accept a list of `(mutant_name, Vec<test_id>)`, dispatch to workers as they become available.
  - Timeout management: per-mutant timeout (default: 10× baseline test duration), send SIGKILL on timeout.
  - Worker crash recovery: detect closed socket, record result, spawn replacement.
  - Graceful shutdown: send `shutdown` to all workers on completion or SIGINT.
- Module `irradiate-core::stats` with:
  - Run full test suite once with `active_mutant = "stats"`.
  - Collect `tests_by_function` and `duration_by_test` from stats plugin output.
  - Save to `.irradiate/stats.json`.
- Integration test: spin up the worker pool against `tests/fixtures/simple.py`, run a few mutants, verify killed/survived classification.

### Success Criteria

- `cargo test` passes including integration test that runs actual pytest workers.
- Worker pool correctly dispatches mutants and collects results over unix sockets.
- Stats collection produces a valid `tests_by_function` mapping.

### Ralph Command

```
/ralph-loop:ralph-loop "Read docs/spec.md and implement Spec 3: Python Harness + Worker Pool" --max-iterations 30 --completion-promise "cargo test passes including worker pool integration tests"
```

---

## Spec 4: CLI + E2E Integration

**Prerequisites:** Specs 1-3

### mutmut compatibility decisions

Match mutmut's CLI surface where it makes sense, use our own where it doesn't:

- **Match:** `run`, `results`, `show` subcommands. `.meta` JSON format per mutated file. `[tool.mutmut]` config section in pyproject.toml. Exit code semantics (0=survived, 1=killed, 33=no tests, 37=type check). Mutant naming (`x_func__mutmut_N`).
- **Skip for now:** `apply`, `browse`, `export-cicd-stats`, `print-time-estimates`, `tests-for-mutant`. `setup.cfg` support. `--max-children` flag (our worker pool is different).
- **Own style:** progress output, summary format.

### Requirements

- CLI via clap with subcommands:
  - `irradiate run` — full pipeline: mutate → stats → validate → test mutants → report.
    - `--paths-to-mutate <path>` (default: auto-detect `src/` or project name dir)
    - `--tests-dir <path>` (default: `tests/`)
    - `--workers <N>` (default: CPU count)
    - `--timeout-multiplier <float>` (default: 10.0)
    - `--no-stats` (skip stats collection, test all mutants against all tests)
    - `--covered-only` (skip mutants with no test coverage)
    - Specific mutant names as positional args (optional — run only these)
  - `irradiate results` — print summary table + list survived mutants.
    - `--all` flag to show all mutants, not just survived.
  - `irradiate show <mutant_name>` — print unified diff of mutant vs original.
- Full pipeline orchestration in `irradiate run`:
  1. Mutation generation — parse Python files, generate trampolined mutants to `mutants/` dir.
  2. Stats collection — run test suite with `active_mutant = "stats"` to map tests→functions.
  3. Validation — clean run (no mutant) must pass; forced fail (`active_mutant = "fail"`) must fail.
  4. Mutation testing — sort by estimated time, dispatch to worker pool, collect results.
  5. Results — write `.meta` files (mutmut-compatible), print summary.
- `.meta` file format (per mutated source file, mutmut-compatible):
  ```json
  {
    "exit_code_by_key": {"module.x_func__mutmut_1": 1, ...},
    "durations_by_key": {"module.x_func__mutmut_1": 0.042, ...}
  }
  ```
- Config: read `[tool.mutmut]` section from `pyproject.toml`. Supported keys: `paths_to_mutate`, `tests_dir`, `do_not_mutate`, `also_copy`, `debug`, `pytest_add_cli_args`. CLI flags override config.
- E2e test: build binary, run against `tests/fixtures/simple_project/`, verify correct killed/survived counts.

### Success Criteria

- `cargo check && cargo clippy -- -D warnings && cargo test` passes.
- `bash tests/e2e.sh` passes end-to-end.
- `irradiate run` on the simple fixture produces correct killed/survived results.
- `irradiate results` prints a readable summary.
- `irradiate show <mutant>` prints a diff.

### Ralph Command

```
/ralph-loop:ralph-loop "Read docs/spec.md and implement Spec 4: CLI + E2E Integration" --max-iterations 30 --completion-promise "cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh all pass"
```
