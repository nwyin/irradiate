# irradiate

Mutation testing for Python, written in Rust. A spiritual successor to [mutmut](https://github.com/boxed/mutmut).

> This document describes the target architecture and design direction. For the narrower implementation contract of the current vertical slice, see `docs/spec.md`. For gaps and follow-up work, see `docs/roadmap.md`.

## Why

Mutation testing is painfully slow. The bottleneck isn't generating mutants — it's running the test suite once per mutant. A typical mutation testing run looks like:

```
for each of 1000 mutants:
    fork process
    start pytest from scratch (200-500ms)
    run relevant tests (50ms)
    collect result
```

pytest startup dominates. The actual test execution is often a fraction of the ceremony around it. irradiate exists to eliminate that ceremony.

### Background

mutmut's original author [described](https://kodare.net/2016/12/01/mutmut-a-python-mutation-testing-system.html) wanting to use Python import hooks to mutate code at runtime, enabling parallelism. He abandoned the approach because Python's import system was too unreliable. The fallback — disk-based mutation with fork-per-mutant — was pragmatic but left performance on the table. He also envisioned a shared mutation results database across developers, which was never built.

irradiate picks up these threads: the trampoline architecture (which mutmut eventually adopted) already enables runtime mutation switching without reimporting. We take it to its conclusion with a pre-warmed worker pool, and we implement the content-addressable cache he envisioned.

## Core idea: pre-warmed worker pool

Instead of paying pytest startup cost per mutant, irradiate maintains a pool of long-lived Python worker processes. Each worker:

1. Starts once
2. Imports pytest, collects all tests, resolves fixtures — once
3. Sits idle until the Rust orchestrator sends it work over a unix socket
4. Receives `(mutant_name, [test_ids])`, sets a global variable, runs the selected test items directly (no re-collection, no `pytest.main()`)
5. Reports exit code and duration back to the orchestrator
6. Waits for the next mutant

This works because of how mutation dispatch works at runtime: mutated files contain a "trampoline" function that checks which mutant is active on every call. The active mutant is just a Python global variable — switching it between test runs requires zero reimporting.

If a worker crashes (segfault, timeout), the orchestrator detects the closed socket, records the result, and spawns a replacement. The pool self-heals.

### Worker pool lifecycle

```
Startup:
  Rust orchestrator spawns N workers (N = cpu count)
  Each worker: connect to unix socket, import pytest, collect tests, report ready

Steady state:
  Orchestrator sends work item → worker sets active_mutant, runs tests, reports result
  Orchestrator tracks wall-clock time per worker, sends SIGXCPU on timeout
  If worker dies → orchestrator detects closed socket, records result, spawns replacement

Shutdown:
  Orchestrator sends shutdown message to all workers
  Workers exit cleanly
```

### IPC protocol

Newline-delimited JSON over unix domain sockets:

```
Orchestrator → Worker:
  {"type":"warmup"}
  {"type":"run","mutant":"my_lib.x_hello__mutmut_1","tests":["tests/test.py::test_hello"]}
  {"type":"shutdown"}

Worker → Orchestrator:
  {"type":"ready","pid":12345}
  {"type":"result","exit_code":1,"duration":0.042}
  {"type":"error","message":"..."}
```

### Fallback: generic test command mode

For non-pytest test runners, irradiate supports `--test-command "make test"`. In this mode, each mutant spawns a fresh subprocess with `MUTANT_UNDER_TEST` set in the environment. No worker pool, no IPC — just fork/exec per mutant. Slower, but works with any test runner that returns exit code 0 for success. This preserves mutmut's original "any test runner" philosophy while allowing deep pytest optimization.

## Architecture

```
irradiate (Rust binary)
├── CLI (clap)
├── Mutation Engine
│   ├── Python parser (libcst Rust crate — same parser as Python's LibCST)
│   ├── Mutation operators (declarative tables + procedural operators)
│   ├── Trampoline code generation
│   └── Parallel file processing (rayon)
├── Worker Pool Orchestrator
│   ├── Spawn/manage N pytest worker processes
│   ├── Unix domain socket IPC (tokio)
│   ├── Timeout management (tokio timers, not sleep-polling)
│   ├── Signal handling (SIGXCPU, SIGKILL, SIGINT)
│   └── Work queue sorted by estimated execution time
├── Cache
│   ├── Content-addressable result store
│   ├── Incremental mutation detection
│   └── Optional shared/remote cache
├── Result Store
│   ├── .meta JSON files (backward-compatible with mutmut)
│   ├── Stats JSON (test-to-mutant mapping, durations)
│   └── Batched writes (not per-mutant like mutmut)
└── TUI Browser (ratatui)
```

A small Python package (`irradiate-harness`) ships alongside the binary. It contains:

- `worker.py` — the pytest worker loop (~100 lines)
- `stats_plugin.py` — pytest plugin for recording which tests cover which functions
- `trampoline.py` — holds the `active_mutant` global that the trampoline reads

## What stays Python

Three things must remain Python because they run inside the test process:

1. **The trampoline** — injected into mutated source files, dispatches function calls based on `active_mutant` global. Uses a module global (fast dict lookup) instead of `os.environ` (syscall per call).
2. **The worker process** — ~100 lines that connects to a unix socket, receives work, calls pytest item execution directly, reports results.
3. **The stats plugin** — a pytest plugin that records which tests execute which trampolined functions.

Everything else — parsing, mutation, orchestration, caching, I/O, CLI, TUI — is Rust.

## Content-addressable cache

mutmut caches results by file modification time: if the source is newer than the mutant file, regenerate. This is fragile — `touch` a file, lose all results. Rebasing drops results even if code didn't change. Results aren't shareable across developers or CI.

irradiate uses content-addressable caching. Each mutation result is keyed by:

```
cache_key = hash(
    function_body_normalized,   # the function source, whitespace-normalized
    mutation_operator_id,       # which operator produced this mutant
    mutation_index,             # which application of that operator
    test_set_hash,              # hash of the sorted test IDs that cover this function
    test_content_hash,          # hash of the test file contents
)
```

If the key matches a previous result, skip the test run entirely. This survives:

- **Reformatting** — whitespace-normalized function body means `black`/`ruff format` doesn't invalidate cache
- **Rebasing** — if the function didn't change, the result holds
- **Branch switching** — same code = same results regardless of branch
- **CI** — upload cache artifacts, download on next run, skip already-tested mutants

### Cache storage

Local cache lives in `.irradiate/cache/` as a directory of small files keyed by hash prefix (similar to git's object store). Optional remote cache via `--cache-url` for team/CI sharing — a simple HTTP GET/PUT interface against S3, GCS, or a shared filesystem.

### Cache invalidation

The cache is naturally self-invalidating: if any input to the hash changes (function body, test code, operator definition), the key changes and the old result is simply never looked up. No explicit invalidation needed. Old cache entries can be garbage-collected by age.

## Mutation operators

### Design: declarative tables over procedural code

mutmut's operators are Python functions that take CST nodes and yield mutated variants. They work, but adding a new operator requires understanding the libcst node API, writing match logic, and handling edge cases.

irradiate splits operators into two categories:

**Table-driven operators** — the majority. Defined as static data:

```rust
// Adding a new swap is one line
static BINARY_OP_SWAPS: &[(BinOp, BinOp)] = &[
    (BinOp::Add, BinOp::Sub),
    (BinOp::Sub, BinOp::Add),
    (BinOp::Mul, BinOp::Div),
    (BinOp::Div, BinOp::Mul),
    (BinOp::Eq, BinOp::NotEq),
    (BinOp::Lt, BinOp::LtE),
    // ...
];

static KEYWORD_SWAPS: &[(Keyword, Keyword)] = &[
    (Keyword::And, Keyword::Or),
    (Keyword::Is, Keyword::IsNot),
    (Keyword::In, Keyword::NotIn),
    (Keyword::Break, Keyword::Return),
    (Keyword::Continue, Keyword::Break),
];

static BOOL_SWAPS: &[(Name, Name)] = &[
    ("True", "False"),
    ("False", "True"),
    ("deepcopy", "copy"),
];

static METHOD_SWAPS: &[(&str, &str)] = &[
    ("lower", "upper"),
    ("upper", "lower"),
    ("lstrip", "rstrip"),
    ("rstrip", "lstrip"),
    ("find", "rfind"),
    ("rfind", "find"),
];
```

A generic walker applies all table-driven operators by matching node types against the tables. No per-operator visitor logic needed.

**Procedural operators** — for mutations that require structural analysis:

- `argument_removal` — needs to inspect arg count, generate N variants
- `string_mutation` — regex-based case swapping, prefix/suffix wrapping
- `lambda_mutation` — body → `None` or `0` depending on current body
- `assignment_mutation` — `a = x` → `a = None`, `a = None` → `a = ""`
- `match_case_removal` — drop each case branch from match statements

These implement a trait:

```rust
trait MutationOperator {
    fn id(&self) -> &str;
    fn mutate(&self, node: &CSTNode, ctx: &MutationContext) -> Vec<CSTNode>;
}
```

### Operator skip rules

Certain patterns should never be mutated:

- `# pragma: no mutate` on the same line
- Lines not in the coverage set (when `--covered-only` is enabled)
- Dunder methods that affect object identity: `__getattribute__`, `__setattr__`, `__new__`
- Calls to `len()`, `isinstance()` — mutations here rarely produce useful signal
- Type annotations
- Decorator expressions
- Docstrings (triple-quoted strings)

### Operator catalog

| Category | Mutations | Type |
|----------|-----------|------|
| Arithmetic | `+` ↔ `-`, `*` ↔ `/`, `//`, `%`, `**` | Table |
| Comparison | `<` ↔ `<=`, `>` ↔ `>=`, `==` ↔ `!=` | Table |
| Logical | `and` ↔ `or` | Table |
| Bitwise | `&` ↔ `\|` ↔ `^`, `<<` ↔ `>>` | Table |
| Boolean | `True` ↔ `False` | Table |
| Keywords | `is` ↔ `is not`, `in` ↔ `not in`, `break` → `return` | Table |
| Unary removal | `not x` → `x`, `~x` → `x` | Table |
| String methods | `.lower()` ↔ `.upper()`, `.lstrip()` ↔ `.rstrip()` | Table |
| Numbers | `n` → `n + 1` | Procedural |
| Strings | case swap, `"XX"` wrapping | Procedural |
| Lambdas | body → `None` | Procedural |
| Arguments | remove args, replace with `None` | Procedural |
| Assignments | `a = x` → `a = None`, `+=` → `=` | Procedural |
| Match | drop case branches | Procedural |

## The trampoline

The trampoline is Python code injected into every mutated source file. For each function with mutations, irradiate generates:

1. The **original function**, renamed to `x_func__mutmut_orig`
2. **N mutated variants**, named `x_func__mutmut_1` through `x_func__mutmut_N`
3. A **mutant lookup dict** mapping variant names to function references
4. A **trampoline wrapper** with the original function name that dispatches at runtime

```python
import irradiate_harness as _ih

def _irradiate_trampoline(orig, mutants, call_args, call_kwargs, self_arg=None):
    active = _ih.active_mutant          # fast global lookup, no syscall
    if not active:                       # hot path: no mutation active
        return orig(*call_args, **call_kwargs)
    if active == 'fail':
        raise _ih.ProgrammaticFailException()
    if active == 'stats':
        _ih.record_hit(orig.__module__ + '.' + orig.__name__)
        return orig(*call_args, **call_kwargs)
    prefix = orig.__module__ + '.' + orig.__name__ + '__mutmut_'
    if not active.startswith(prefix):    # not our function
        return orig(*call_args, **call_kwargs)
    variant = active.rpartition('.')[-1]
    if self_arg is not None:
        return mutants[variant](self_arg, *call_args, **call_kwargs)
    return mutants[variant](*call_args, **call_kwargs)
```

Key properties:
- Reads a module global (`_ih.active_mutant`), not `os.environ` — avoids a syscall on every instrumented function call
- The hot path (no mutation active) is a single falsy check
- Dispatch is a dict lookup, not a chain of if-statements
- Compatible with mutmut's naming convention (`__mutmut_N` suffixes, `ǁ` class separator)

## Execution phases

### Phase 1: Mutation generation

```
for each .py file in paths_to_mutate (parallel via rayon):
    parse with libcst Rust crate
    walk CST, apply operators, collect mutations grouped by function
    for each function with mutations:
        emit original (renamed), variants, lookup dict, trampoline wrapper
    write combined mutated source to mutants/
    write .meta stub (mutant names, no results yet)
```

### Phase 2: Stats collection

Run the full test suite once with `active_mutant = "stats"`. The trampoline calls `record_hit()` for every instrumented function, building:

- `tests_by_function`: which tests exercise which functions
- `duration_by_test`: how long each test takes

Saved to `mutants/irradiate-stats.json`. On subsequent runs, only re-collect stats for new or changed tests.

### Phase 3: Validation

- **Clean run**: all tests pass with no mutant active
- **Forced fail**: `active_mutant = "fail"` — at least one test must fail (verifies trampoline is wired up)

### Phase 4: Mutation testing

```
sort mutants by estimated execution time (fastest first)
for each mutant:
    check cache → if hit, record result, skip
    check type checker → if caught, record exit_code=37, skip
    if no tests cover this function → record exit_code=33, skip
    send to next available worker
    worker sets active_mutant, runs tests with -x (fail-fast)
    worker reports exit_code and duration
    orchestrator records result, updates cache
```

### Phase 5: Results

Aggregate results from all `.meta` files. Classify each mutant:

| Exit code | Status | Meaning |
|-----------|--------|---------|
| 0 | survived | Tests passed — mutant not caught |
| 1 | killed | Tests failed — mutant detected |
| 33 | no tests | No tests cover this function |
| 37 | type check | Caught by static type checker |
| timeout | timeout | Tests exceeded time limit |
| -11 | segfault | Worker crashed |

## Where the speed comes from

1. **No pytest startup per mutant** — workers collect once, run many times. For a 200ms pytest startup with 1000 mutants, this saves ~200 seconds.
2. **Content-addressable cache** — unchanged functions with unchanged tests skip entirely. On incremental runs (one file changed), most mutants are cache hits.
3. **No `os.environ` syscall in the trampoline** — module global instead of env var lookup on every function call. For code with thousands of instrumented function calls per test, this adds up.
4. **Parallel mutation generation** — rayon, no GIL, no multiprocessing.Pool overhead.
5. **Batched result I/O** — not a JSON write after every single mutant (mutmut has a TODO comment acknowledging this bottleneck).
6. **Native orchestration** — tokio manages sockets, timers, signals without GIL contention.

## Compatibility with mutmut

irradiate produces the same output format as mutmut:

- Same mutant naming convention (`module.xǁClassǁmethod__mutmut_N`)
- Same `.meta` JSON files in `mutants/`
- Same `mutmut-stats.json` format
- Same trampoline architecture (function renaming + dispatch)
- Same `# pragma: no mutate` support
- Same `[tool.mutmut]` config section in `pyproject.toml`

Correctness is validated by running both tools against the same projects and diffing the results.

### Compatibility test suite

- mutmut's `e2e_projects/` (basic coverage)
- Real-world projects for stress testing: a Django app, a data pipeline, a CLI tool
- For each: run mutmut, run irradiate, diff the mutant names and exit codes

## Risks and mitigations

**Test state leakage in worker pool**: Reusing a pytest process means global state from one test run could affect the next. Mitigations:
- Respawn workers every N mutants (configurable, default 100)
- `--isolate` flag falls back to one-shot workers (subprocess per mutant)
- `-x` fail-fast means most runs touch minimal state
- Workers can optionally create fresh pytest sessions between runs (more overhead, better isolation)

**libcst Rust crate gaps**: The Rust crate may not cover all Python syntax that the Python LibCST handles. Mitigations:
- Fall back to copying files unchanged when parsing fails (same as mutmut)
- Track which syntax constructs cause parse failures, file issues upstream
- The libcst Rust crate is the same parser that powers the Python LibCST — it's actively maintained and handles Python 3.0-3.14

**pytest internal state accumulation**: Fixtures and plugins may accumulate state across runs within a single worker. Mitigations:
- Reset session state between runs
- Monitor worker memory usage, respawn if it grows beyond a threshold
- Document known incompatible pytest plugins

**Cache correctness**: A bad hash function could cause false cache hits (reusing results for changed code). Mitigations:
- Use SHA-256 for content hashing
- Include the irradiate version in the cache key (operator changes invalidate cache)
- `--no-cache` flag for debugging
- Cache entries are never updated, only created — eliminates race conditions
