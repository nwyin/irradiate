# irradiate — Roadmap

Status as of 2026-03-19. The core pipeline is complete and working on real-world projects (markupsafe, click, my_lib, synth). This document tracks remaining work.

## Verification

```bash
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
```

---

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

---

## Design Decision: Worker Execution Model

### Current state

The harness now has two execution paths:

- **Legacy path**: call `pytest.main(["-x", "--no-header", "-q"] + test_ids)` for every mutant. This re-invokes pytest's full startup sequence — argument parsing, plugin loading, test collection, fixture resolution — then runs the selected tests and exits.
- **Fast path**: keep a pytest session alive inside the worker, collect once, then execute pre-collected items directly for each mutant.

Today the fast path proves the performance model, but it currently does so by reaching into pytest internals (`_pytest.runner.runtestprotocol`, `_setupstate`, per-item report buffers). So the design question is no longer "should we have a direct execution path?" but "which fast execution architecture do we want to standardize and harden?"

### Why this exists

This was a pragmatic shortcut to get the vertical slice working. `pytest.main()` is the only *public* API for running tests. It's well-documented, handles all edge cases (plugin lifecycle, fixture teardown, output capture, exit codes), and is guaranteed stable across pytest versions. Going deeper into pytest's internals trades stability for performance.

### The performance cost

On a typical project with 200ms pytest startup and 1000 mutants:
- **Current**: 1000 × 200ms = 200 seconds of pure startup overhead
- **Direct execution**: 1 × 200ms + 1000 × (test time only) ≈ 200ms + test time
- For fast tests (50ms each), this is the difference between 250s and 50s — a 5× speedup

Without direct execution, the worker pool is just a process pool with warm Python interpreters, saving only the Python startup time (~50ms), not the pytest startup time (~200ms).

### Fast execution options

#### Option 1: Warm session + hook-driven execution

This keeps the current worker shape:

1. Start pytest once per worker
2. Collect all tests once
3. Own `pytest_runtestloop` inside a worker plugin
4. Receive `(mutant_name, [test_ids])` over IPC
5. Resolve nodeids to pre-collected `Item` objects
6. Execute each item through pytest's hook machinery (`pytest_runtest_protocol`, `pytest_runtest_logreport`) instead of importing `_pytest.runner` directly

This is closest to how `pytest-xdist` workers behave: each worker is a miniature pytest runner that owns collection and executes selected items as the controller sends them work.

**Pros**
- Preserves the main performance win: startup/collection paid once per worker
- Smallest implementation delta from the current worker/orchestrator design
- Keeps the existing nodeid-based IPC model
- Moves the execution path closer to documented pytest hook surfaces

**Cons**
- Session-scoped fixtures, module globals, and plugin state still live across mutants
- Some plugins assume one pytest session equals one logical run
- We may still need careful cleanup or selective recycling between runs

#### Option 2: Fork snapshot from a warm collected parent

This is a Unix-only design, but Windows support is not a requirement for this project.

1. Start pytest once in a parent worker process
2. Collect all tests once
3. Leave the parent idle as a "clean enough" snapshot
4. `fork()` a child per mutant or per small batch
5. In the child, set `active_mutant`, run the selected items, report results, and exit
6. The parent never accumulates post-run test state because each mutant runs in a short-lived child

**Pros**
- Much stronger isolation between mutants
- Better crash containment: child crashes do not poison the parent collector
- Retains much of the startup/collection win because the address space is inherited via `fork`

**Cons**
- Higher implementation complexity than the warm-session model
- Higher per-mutant overhead than direct in-session execution
- Forking an already-initialized pytest process can have plugin- and platform-specific sharp edges, especially if plugins start threads, register process-global resources, or otherwise assume no post-initialization fork
- Still not as correct or portable as full `--isolate`

### Shared requirements for any fast path

Regardless of which fast architecture we standardize, the worker must handle:

- **Fixture teardown semantics**: preserve pytest's expected setup/teardown ordering for selected items
- **Captured output reset**: no stdout/stderr leakage between mutant runs
- **Plugin state accumulation**: warnings, coverage, junitxml, and custom plugins may keep mutable session state
- **Result collection**: convert test outcomes back into a simple killed/survived/error signal for the orchestrator
- **Fallbacks**: keep `--isolate` and worker recycling available when a suite is incompatible with the fast path

### Risks

- **Private pytest internals are a maintenance hazard**: the current fast path depends on underscored pytest APIs and internal state containers. Mitigation: move the primary implementation toward public hook surfaces where possible.
- **State leakage remains the core semantic risk for warm-session execution**: session fixtures, module globals, and plugin state survive between mutants. Mitigation: worker recycling (already implemented via `--worker-recycle-after`) and `--isolate`.
- **Fork safety is the core semantic risk for snapshot/fork execution**: some plugins and runtimes behave poorly if pytest is forked after initialization. Mitigation: treat fork-snapshot as an explicit backend with compatibility testing, not as the only execution mode.
- **Plugin compatibility remains a product concern either way**: some plugins assume one session = one run, others may assume no post-init fork. Mitigation: document incompatible plugins, keep multiple backends, and bias toward conservative fallbacks when behavior is unclear.
- **Fixture teardown ordering still matters**: running items out of collection order may trigger fixtures in unexpected sequences. Mitigation: run items in original collection order within each mutant.

### Recommendation

If we have to pick one fast architecture as the default product path, the best next step is:

1. **Standardize on Option 1**: warm session + hook-driven execution
2. Replace direct imports of `_pytest.runner.runtestprotocol` with pytest hook calls where feasible
3. Treat worker recycling as part of the default correctness story, not just a debugging escape hatch
4. Keep `--isolate` as the strongest fallback
5. Consider Option 2 later as an experimental Unix-only backend if real-world suites show too much leakage under the warm-session model

Why this is the recommended default:

- It preserves nearly all of the performance upside
- It fits the current worker/orchestrator architecture with minimal churn
- It reduces dependency on private pytest internals without changing the core execution model
- It avoids making post-initialization `fork()` behavior the foundation of the product

### Phased approach

1. **Phase 1 (done)**: `pytest.main()` per mutant. Correct but slow. Validates the pool architecture.
2. **Phase 2 (prototype exists, needs hardening)**: direct execution on pre-collected items inside a long-lived worker session. This is the main performance win, but the current implementation still leans on private pytest internals.
3. **Phase 3 (next)**: rework the fast path around pytest's hook machinery and tighten state-reset/reporting semantics so the default backend is not built around underscored imports.
4. **Phase 4 (done)**: Worker recycling via `--worker-recycle-after`. Bounds state leakage.
5. **Phase 5 (done)**: `--isolate` flag. Fresh subprocess per mutant for max correctness.
6. **Phase 6 (optional, later)**: evaluate a Unix-only fork-snapshot backend as a middle ground between warm-session speed and `--isolate` correctness.

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

The mutation engine uses text-based CST walking for discovery and applies mutations through text substitution. Repeated identical tokens inside one function can map to the wrong source slice.

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

| # | Item | Effort | Impact | Status |
|---|------|--------|--------|--------|
| 1 | Direct test execution via `runtestprotocol()` | L | The core performance win — 5-10× on real projects | |
| 2 | Content-addressable cache | L | Big perf win on incremental runs | |
| 3 | Worker pool hardening (hot-accept, memory, scheduling) | L | Robustness at scale | |
| ~~4~~ | ~~Skip rule gaps~~ | S | ~~Correctness~~ | done — per-line pragma, type annotations, len/isinstance, do_not_mutate all implemented |
| ~~5~~ | ~~Static analysis artifacts~~ | S | ~~Aids contributors~~ | done — `docs/artifacts/` |
