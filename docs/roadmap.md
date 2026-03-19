# irradiate — Roadmap

Status as of 2026-03-18. The core pipeline is complete and working on real-world projects (markupsafe, click, my_lib, synth). This document tracks remaining work.

## Verification

```bash
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
```

---

## Missing Operators

Table-driven, quick to add:

| Operator | Type | Notes |
|----------|------|-------|
| String method swaps | Table | `.lower()↔.upper()`, `.lstrip()↔.rstrip()`, `.find()↔.rfind()` |
| Argument removal | Procedural | Remove each arg, replace with `None`. Need to inspect arg count, generate N variants. |
| Match case removal | Procedural | Drop each `case` branch from `match` statements. Python 3.10+. |

## Content-addressable Cache

Biggest remaining feature. Each mutation result keyed by:

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

## Worker Pool Hardening

- **Hot-accept after respawn**: respawned workers can't rejoin the accept loop (comment in `orchestrator.rs` line ~379).
- **Memory monitoring**: respawn workers that exceed a configurable memory threshold.
- **Scheduler improvements**: use collected test durations for ordering and per-mutant timeout budgets.

Note: `--isolate` mode and `--worker-recycle-after` are already implemented.

## Skip Rule Gaps

- Type annotations not skipped (should never mutate hints).
- `len()` / `isinstance()` calls not skipped (mutations rarely produce useful signal).
- `# pragma: no mutate` collected but not enforced per-expression by line number — only per-function.

## Dogfooding: Mutation Testing irradiate Itself

Use irradiate to mutation-test its own Python harness files (`harness/__init__.py`, `worker.py`, `stats_plugin.py`):

- Run `irradiate run` from the repo root with `--paths-to-mutate harness/`.
- Write dedicated pytest tests for the harness if coverage is weak.
- Track mutation score over time — add to CI as a quality gate once stable.

## Static Analysis Artifacts

Generate artifacts for human reviewers:

```
docs/artifacts/
├── module-deps.svg       # Rust module dependency graph
├── call-stack.svg        # Binary call graph (top-level entry points)
├── dep-graph.svg         # Crate dependency graph
├── worker-cfg.svg        # Python worker control flow
└── README.md             # How to regenerate these
```

Regenerate on major refactors. Don't automate in CI — these are for human review during design discussions.

---

## Design Decision: Worker Execution Model

### Current approach

Workers call `pytest.main(["-x", "--no-header", "-q"] + test_ids)` for every mutant. This re-invokes pytest's full startup sequence — argument parsing, plugin loading, test collection, fixture resolution — then runs the selected tests and exits. The pre-collected test items from the `ItemCollector` plugin are used only for reporting available tests to the orchestrator, not for execution.

### Why this exists

This was a pragmatic shortcut to get the vertical slice working. `pytest.main()` is the only *public* API for running tests. It's well-documented, handles all edge cases (plugin lifecycle, fixture teardown, output capture, exit codes), and is guaranteed stable across pytest versions. Going deeper into pytest's internals trades stability for performance.

### The performance cost

On a typical project with 200ms pytest startup and 1000 mutants:
- **Current**: 1000 × 200ms = 200 seconds of pure startup overhead
- **Direct execution**: 1 × 200ms + 1000 × (test time only) ≈ 200ms + test time
- For fast tests (50ms each), this is the difference between 250s and 50s — a 5× speedup

Without direct execution, the worker pool is just a process pool with warm Python interpreters, saving only the Python startup time (~50ms), not the pytest startup time (~200ms).

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

### Risks

- **Pytest internal API instability**: `_pytest.runner.runtestprotocol` is private API. Mitigation: version-check at startup, fall back to `pytest.main()` on unrecognized versions.
- **State leakage**: session-scoped fixtures, module-level variables, and global state survive between runs. Mitigation: worker recycling (respawn every N mutants, already implemented via `--worker-recycle-after`).
- **Plugin compatibility**: some pytest plugins assume one session = one run. Mitigation: document incompatible plugins, offer `--isolate` fallback (already implemented).
- **Fixture teardown ordering**: running items out of collection order may trigger fixtures in unexpected sequences. Mitigation: run items in their original collection order within each mutant.

### Phased approach

1. **Phase 1 (done)**: `pytest.main()` per mutant. Correct but slow. Validates the pool architecture.
2. **Phase 2 (next)**: `runtestprotocol()` on pre-collected items. The main performance win. Add version check + fallback.
3. **Phase 3 (done)**: Worker recycling via `--worker-recycle-after`. Bounds state leakage.
4. **Phase 4 (done)**: `--isolate` flag. Fresh subprocess per mutant for max correctness.

---

## Design Review Notes

Architectural feedback recorded after the implementation reached end-to-end working state on real-world codebases.

### What looks strong

- The Rust/Python split is good. Rust owns mutation planning, orchestration, reporting, and I/O; Python stays limited to the runtime pieces that must execute inside the test process.
- The trampoline approach is the right performance-oriented design. Switching mutants through a global `active_mutant` is the key idea.
- The codebase is small and understandable. `pipeline`, `mutation`, `codegen`, `trampoline`, `stats`, `orchestrator`, and the Python harness each have clear ownership.
- Vendor testing on real projects (markupsafe, click, my_lib) has proven the architecture handles real-world Python patterns: `super()`, generators, async, decorators, multi-line signatures, `from __future__` imports, class methods.

### Main critiques

#### Mutation application is the biggest correctness risk

The mutation engine uses text-based CST walking for discovery and applies mutations through text substitution. Repeated identical tokens inside one function can map to the wrong source slice, and `# pragma: no mutate` is not yet enforced at expression granularity.

#### The worker pool doesn't yet realize its full performance potential

Workers still call `pytest.main(test_args)` for every mutant. The pre-warmed pool saves Python interpreter startup (~50ms) but not pytest startup (~200ms). Moving to `runtestprotocol()` direct execution is the key remaining performance win.

#### Worker lifecycle hardening is partially done

- `--isolate` mode works.
- `--worker-recycle-after` bounds state leakage.
- Timeouts are still coarse — no per-mutant timeout budgets from stats.
- Respawned workers cannot rejoin the pool.
- The scheduler does not yet use collected test durations for ordering.

### Recommended sequencing

1. Tighten mutation correctness: move from substring-based application toward stable spans or structured rewriting.
2. Make worker execution semantics more honest: move to `runtestprotocol()` direct execution.
3. Use collected stats for actual scheduling and per-mutant timeout budgets.
4. Expand operator coverage and cache only after the foundation above is solid.

---

## Priority Order

| # | Item | Effort | Impact |
|---|------|--------|--------|
| 1 | Direct test execution via `runtestprotocol()` | L | The core performance win — 5-10× on real projects |
| 2 | Content-addressable cache | L | Big perf win on incremental runs |
| 3 | String method swap operators | S | More mutants, quick table addition |
| 4 | Skip rule gaps (type hints, pragma per-expression) | S | Correctness — fewer false-positive mutants |
| 5 | Worker pool hardening (hot-accept, memory, scheduling) | L | Robustness at scale |
| 6 | Argument removal / match case operators | M | More complete operator coverage |
| 7 | Dogfooding (mutation testing own harness) | M | Quality gate, validates harness correctness |
| 8 | Static analysis artifacts | S | Aids contributors and reviewers |
