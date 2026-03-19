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

This area is now largely complete for the current worker architecture:

- **Hot-accept after respawn**: replacement workers rejoin the accept loop correctly.
- **Memory monitoring**: workers can be recycled when RSS exceeds a configurable threshold.
- **Duration-aware scheduling**: collected test durations now feed both per-mutant timeout budgets and longest-first work ordering.
- **Fallbacks**: `--isolate` and `--worker-recycle-after` are both implemented.

Remaining work here is mostly empirical tuning and plugin-compatibility coverage rather than missing core mechanisms.

---

## Design Decision: Worker Execution Model

### Current state

Today there are two user-visible execution modes:

- **Default worker-pool mode**: keep a pytest session alive inside each worker, collect once, then execute pre-collected items directly for each mutant.
- **`--isolate` mode**: run each mutant in a fresh subprocess for maximum correctness at the cost of startup overhead.

The fast path now uses pytest hook machinery for execution inside the long-lived worker session rather than importing `_pytest.runner.runtestprotocol` directly. The remaining design question is less about "can we avoid private pytest internals?" and more about how aggressively we want to harden compatibility and fallbacks around the warm-session model.

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

- **Private pytest internals were a maintenance hazard**: the earlier prototype depended on underscored pytest APIs and internal state containers. Mitigation: the default fast path now uses pytest hooks instead, and future changes should keep private dependencies isolated and explicit.
- **State leakage remains the core semantic risk for warm-session execution**: session fixtures, module globals, and plugin state survive between mutants. Mitigation: worker recycling (already implemented via `--worker-recycle-after`) and `--isolate`.
- **Fork safety is the core semantic risk for snapshot/fork execution**: some plugins and runtimes behave poorly if pytest is forked after initialization. Mitigation: treat fork-snapshot as an explicit backend with compatibility testing, not as the only execution mode.
- **Plugin compatibility remains a product concern either way**: some plugins assume one session = one run, others may assume no post-init fork. Mitigation: document incompatible plugins, keep multiple backends, and bias toward conservative fallbacks when behavior is unclear.
- **Fixture teardown ordering still matters**: running items out of collection order may trigger fixtures in unexpected sequences. Mitigation: run items in original collection order within each mutant.

### Recommendation

The project has now standardized on Option 1 as the default fast path:

1. **Warm session + hook-driven execution** is the default architecture
2. Worker recycling is part of the normal correctness story, not just a debugging escape hatch
3. `--isolate` remains the strongest fallback
4. Option 2 remains a later Unix-only experiment if real-world suites show too much leakage under the warm-session model

Why this is the recommended default:

- It preserves nearly all of the performance upside
- It fits the current worker/orchestrator architecture with minimal churn
- It reduces dependency on private pytest internals without changing the core execution model
- It avoids making post-initialization `fork()` behavior the foundation of the product

### Phased approach

1. **Phase 1 (done)**: `pytest.main()` per mutant. Correct but slow. Validated the pool architecture.
2. **Phase 2 (done, historical prototype)**: direct execution on pre-collected items inside a long-lived worker session proved the performance model.
3. **Phase 3 (done)**: the fast path now runs through pytest's hook machinery and no longer depends on direct `_pytest.runner` imports.
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

#### Warm-session execution now exists, but compatibility remains the real risk

The worker pool now executes pre-collected items inside a long-lived pytest session through pytest's hook machinery. The main remaining risk in this area is not raw performance, but semantic leakage across mutants: session-scoped fixtures, mutable plugin state, and suites that assume one pytest session equals one logical run.

#### Worker lifecycle hardening is mostly done

- `--isolate` mode works.
- `--worker-recycle-after` bounds state leakage.
- Per-mutant timeout budgets are derived from collected test durations.
- Respawned workers can rejoin the pool.
- The scheduler now uses collected test durations for longest-first ordering.
- Remaining concerns are empirical: compatibility testing, tuning recycle defaults, and deciding whether a fork-snapshot backend is worth the complexity.

### Recommended sequencing

1. Tighten mutation correctness: move from substring-based application toward stable spans or structured rewriting.
2. Build the content-addressable cache.
3. Expand compatibility coverage for the warm-session worker and keep `--isolate` as the conservative fallback.
4. Expand operator coverage after the foundation above is solid.

---

## Priority Order

| # | Item | Effort | Impact | Status |
|---|------|--------|--------|--------|
| 1 | Content-addressable cache | L | Big perf win on incremental runs | |
| 2 | Mutation application correctness | L | Prevents incorrect source rewrites on repeated identical spans | |
| 3 | Warm-session compatibility hardening | M | Better behavior on complex pytest/plugin ecosystems | |
| ~~4~~ | ~~Direct test execution via hook-driven worker~~ | L | ~~The core performance win — 5-10× on real projects~~ | done |
| ~~5~~ | ~~Worker pool hardening (respawn, memory, scheduling, timeouts)~~ | L | ~~Robustness at scale~~ | done |
| ~~6~~ | ~~Skip rule gaps~~ | S | ~~Correctness~~ | done — per-line pragma, type annotations, len/isinstance, do_not_mutate all implemented |
| ~~7~~ | ~~Static analysis artifacts~~ | S | ~~Aids contributors~~ | done — `docs/artifacts/` |
