---
title: Performance Benchmarks — irradiate vs mutmut
description: Benchmark results comparing irradiate and mutmut on real Python projects. Wall-clock times, mutants per second, and memory usage.
---

# Performance

irradiate and mutmut 3.5.0 share a trampoline architecture and both use coverage-based test filtering. The differences are in orchestration, operator coverage, and memory management. This page presents benchmark results on real-world Python projects, explains why the numbers look the way they do, and helps you decide which tradeoffs matter for your codebase.

## Benchmark setup

All runs used an 8 vCPU / 16 GB RAM DigitalOcean droplet (Ubuntu 24.04) with irradiate v0.3.0 and mutmut 3.5.0 (latest). Each configuration ran 3 timed iterations plus 1 discarded warmup; the median is reported. Both tools' caches were wiped between every timed run (worst-case cold start), and each target used its own isolated virtualenv. We tried to give both tools a fair comparison, including configuring `pytest_add_cli_args_test_selection` to exclude pre-existing test failures that blocked mutmut's stats collection.

Benchmarks are reproducible. See [nwyin/irradiate#33](https://github.com/nwyin/irradiate/issues/33) for raw data and instructions.

## Architecture comparison

Both tools run the test suite once upfront to map which tests cover which functions (stats collection), then for each mutant, run only the mapped tests. The performance difference comes from how they execute that loop.

mutmut loads everything into one Python process (pytest, your source code, all test fixtures), calls `gc.freeze()`, then forks that process for each mutant. Each fork inherits the fully-loaded state via copy-on-write. The parent keeps `max_children` forks running at all times, blocking on `os.wait()` only when at capacity — so it saturates cores well during steady-state. All mutation variants for every function are compiled into a single module per file and held in the parent's memory before forking, which contributes to high memory usage on larger codebases. Mutants are processed in file order with no duration-based scheduling. This architecture is simple and fast on small projects but has downsides: macOS fork crashes ([boxed/mutmut#446](https://github.com/boxed/mutmut/issues/446)), high memory on large codebases, and the stats phase hard-exits if any test fails.

irradiate uses a Rust orchestrator that manages a pool of pre-warmed Python workers (default: one per CPU core). Workers connect over a Unix socket and receive mutant assignments as JSON messages. On Linux, each mutant runs in a forked child process (same `gc.freeze()` + fork isolation as mutmut). On macOS, tests run in-process within each worker by default (`--no-fork`) to avoid kernel panics from memory pressure. The orchestrator dispatches work from a priority queue (longest-estimated mutants first, so short mutants fill the tail), with per-mutant timeouts, memory recycling (workers exceeding a configurable RSS limit are replaced), and content-addressed result caching. This adds ~1.6s of startup overhead (worker pool warmup + trampoline validation) but enables duration-aware scheduling and crash recovery. Total system memory is the orchestrator (~50MB) plus N worker processes (each 100-400MB depending on project size), so a 10-worker run can use 1-4GB in practice.

## Results

The two tools generate different numbers of mutants — irradiate has 38 operator categories vs mutmut's 14 — so raw wall-clock time is not an apples-to-apples comparison. Tables include ms/mutant (wall time ÷ mutant count) to normalize for this difference, though even that metric isn't perfect since different operators produce mutations of varying difficulty for test suites to detect.

In the tables, *8w* = 8 irradiate workers, *8c* = 8 mutmut child processes.

### Small projects: markupsafe and itsdangerous

| Project | irradiate (8w) | mutmut (8c) | irradiate mutants | mutmut mutants | irradiate ms/mut | mutmut ms/mut |
|---|---|---|---|---|---|---|
| **markupsafe** | 15.2s | 6.4s | 386 | 310 | 39.4 | 20.7 |
| **itsdangerous** | 58.7s | 30.5s | 768 | 526 | 76.4 | 57.9 |

mutmut is faster per-mutant on these smaller projects. The gap has two sources:

First, irradiate pays ~1.6s of fixed startup overhead (worker pool warmup + trampoline validation) that gets amortized across every mutant. On markupsafe's 386 mutants, that alone adds ~4 ms/mutant. mutmut's stats collection is cheaper because it runs in-process rather than coordinating external workers.

Second, irradiate has per-mutant IPC overhead (~1–2ms) from JSON serialization over Unix sockets. mutmut communicates via exit codes only, no socket round-trip overhead. This overhead is small in absolute terms but it's a constant cost that doesn't shrink with project size.

On projects with fast test suites, both costs work against irradiate. Startup is large relative to total work, and async dispatch doesn't help when each test run takes milliseconds. But both costs amortize on larger codebases, where orchestration efficiency and memory management matter more.

### Medium project: toolz

| Project | irradiate (8w) | mutmut (8c) | irradiate mutants | mutmut mutants | irradiate ms/mut | mutmut ms/mut |
|---|---|---|---|---|---|---|
| **toolz** | 125.4s | 805.2s | 2,749 | 10,441 | 45.6 | 77.1 |

On toolz, irradiate is 6.4x faster on wall clock and 1.7x faster per-mutant. Several factors contribute:

**Mutant count.** mutmut generates 3.8x more mutants (10,441 vs 2,749). Even at identical per-mutant speed, mutmut would take nearly 4x longer on wall clock. irradiate's Kaminski ROR reduction and curated operator set avoid generating equivalent or redundant mutants — fewer mutants, but a higher proportion are meaningful.

**Scheduling.** irradiate's priority queue dispatches longest-estimated mutants first, so the tail of the run is filled with short mutants and all workers stay busy until the end. mutmut processes mutants in file order — if a slow mutant happens to be last, the remaining cores idle while it finishes. For steady-state throughput with thousands of similar-duration mutants, both tools saturate cores well; the difference shows up in tail latency when per-mutant durations vary.

**Memory.** The irradiate orchestrator process peaks at ~400 MB, but total system memory includes the worker pool — with 8 workers on toolz, total RSS across all processes was ~2-3 GB. mutmut reaches 11.9 GB in a single process. mutmut compiles all mutation variants for every function into a single module per file and holds them in the parent process before forking. With 10,441 mutants, this means thousands of variant function bodies pinned in memory by `gc.freeze()`. At 11.9 GB, cores spend time blocked on COW page faults and swap I/O rather than running tests. irradiate generates trampoline code per-file and doesn't hold all variants in one process. Workers that exceed a configurable RSS limit (default 1024MB on macOS, off on Linux) are recycled, which bounds per-worker growth but not total pool footprint. On memory-constrained machines, reduce `--workers`.

**Mutation score.** irradiate achieves 88% vs mutmut's 78%. This likely reflects irradiate's Kaminski-reduced operator set avoiding equivalent mutants, though we haven't confirmed this with equivalent-mutant analysis.

Getting mutmut to run on toolz required excluding two pre-existing test failures in the upstream toolz repo (`test_curried_operator`, `test_curried_namespace`) via `pytest_add_cli_args_test_selection`. Without these exclusions, mutmut's stats collection aborts.

### Projects where mutmut could not complete

| Project | irradiate (8w) | mutmut outcome |
|---|---|---|
| **marshmallow** | 386.8s (3,659 mutants, 55% score) | Hangs during stats collection |
| **click** | 642.2s | Stats pass, but "could not find any test case for any mutant" |

These are compatibility failures, not performance issues — but they matter for tool selection.

We configured mutmut with per-project venvs, test exclusion flags, and multiple re-runs. marshmallow and click could not be made to work:

On marshmallow, mutmut generates mutants successfully but hangs indefinitely during the stats collection phase. On click, mutmut's stats collection passes clean (1,319 tests pass) but then reports it cannot map any tests to any mutants — likely a limitation of mutmut's trampoline instrumentation with click's src-layout.

mutmut 3.5.0 hard-exits if any test fails during stats collection ([source](https://github.com/boxed/mutmut/blob/master/mutmut/__main__.py), [#336](https://github.com/boxed/mutmut/issues/336), [#485](https://github.com/boxed/mutmut/issues/485)). This is by design — the maintainer's position is that mutation testing requires a green baseline. Users can work around specific test failures with `pytest_add_cli_args_test_selection`, but issues like the click mapping failure and the marshmallow hang have no workaround.

irradiate completed on all 5 targets.

## Choosing between them

For projects with fewer than 500 mutants and a fast test suite, both tools work well. mutmut will finish faster due to lower per-mutant overhead.

For projects with 1000+ mutants, irradiate's duration-aware scheduling and lower per-process memory keep it fast where mutmut's single-process memory footprint starts to hurt. On toolz, this is the difference between 125s and 805s.

For projects with pre-existing test failures or complex layouts (src-layout, namespace packages), irradiate is more likely to work out of the box. mutmut's stats phase hard-exits on any test failure.

On macOS, mutmut has known fork crashes on macOS 13.2+ ([boxed/mutmut#446](https://github.com/boxed/mutmut/issues/446)). irradiate defaults to `--no-fork` mode on macOS, running tests in-process within each worker instead of forking per mutant. This avoids the memory pressure that can cause macOS kernel panics (macOS has no OOM killer — exhausting memory crashes the entire system). Workers are also recycled at 1024MB RSS by default on macOS. Use `--fork` to override if your project needs fork isolation and you have sufficient RAM.

For CI pipelines, irradiate provides `--fail-under`, GitHub Actions annotations, Stryker JSON reports, and `--diff` for incremental runs.

## Methodology notes

irradiate generates more mutants than mutmut (38 operator categories vs 14), so wall-clock comparisons are inherently unequal. The ms/mutant metric normalizes for mutant count, but even that isn't perfectly apples-to-apples because different operators produce mutations of varying "difficulty" for test suites to detect.

Both tools' caches were wiped before each timed run. In real-world usage, both tools cache results across runs. irradiate also caches stats via content fingerprinting, so the second run on unchanged source skips the 1.6s stats phase entirely.

Benchmark source and reproduction instructions: [nwyin/irradiate#33](https://github.com/nwyin/irradiate/issues/33).
