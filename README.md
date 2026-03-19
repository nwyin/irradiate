# irradiate

Mutation testing for Python, written in Rust.

> **Pre-alpha software.** Under very active development — not ready for production use. APIs, output formats, and behavior will change without notice.

## Why

Mutation testing is slow. The bottleneck isn't generating mutants — it's running the test suite once per mutant. irradiate eliminates the overhead by maintaining a pool of pre-warmed pytest worker processes that skip startup costs entirely.

## How it works

1. Parse Python source with [libcst](https://github.com/Instagram/LibCST) (Rust crate)
2. Generate trampolined mutants — each function gets an original copy, N mutated variants, and a runtime dispatcher
3. Collect test coverage stats in a single pytest run
4. Dispatch mutants to a pool of long-lived worker processes over unix sockets
5. Workers set a global variable to activate a mutant, run the relevant tests, report back

No pytest startup per mutant. No reimporting. Just a dict lookup and a function call.

## Usage

```bash
# Run mutation testing
irradiate run

# See results
irradiate results

# Show diff for a specific mutant
irradiate show module.x_func__mutmut_1
```

## What's different from mutmut

| | mutmut | irradiate |
|---|---|---|
| **Startup cost** | `pytest.main()` per mutant (~200ms each) | Pre-warmed worker pool — pytest starts once, runs many |
| **Cache** | mtime-based (breaks on rebase, touch, branch switch) | Content-addressable (SHA-256 of function body + tests + operator) |
| **Orchestration** | Python multiprocessing | Rust + tokio async (no GIL, native signal/timeout handling) |
| **Mutation dispatch** | `os.environ` lookup per call (syscall) | Module global lookup (dict access, no syscall) |
| **Mutation generation** | Sequential Python (LibCST) | Parallel Rust (libcst crate + rayon) |
| **Result I/O** | JSON write per mutant | Batched writes |
| **Isolation** | Fork per mutant only | Default warm-session + `--isolate` flag for full subprocess isolation |
| **State leakage** | None (fresh process per mutant) | Module snapshot/restore between runs, session-fixture-aware recycling, `--verify-survivors` safety net |
| **Worker health** | — | Memory monitoring, automatic respawn, configurable recycling |
| **Test selection** | Coverage-based | Coverage-based + duration-aware scheduling (longest-first ordering, per-mutant timeout budgets) |

## Status

Pre-alpha. The full pipeline works end-to-end on real projects (markupsafe, click).

What's missing:
- TUI browser
- `--test-command` fallback for non-pytest runners
- Remote/shared cache
- Type checker integration
- Lots of edge cases

## Building

```bash
cargo build --release
```

Requires Rust 1.70+ and a Python 3.10+ environment with pytest installed.

## Acknowledgments

irradiate's trampoline architecture and mutation operator design are informed by [mutmut](https://github.com/boxed/mutmut). The output format is partially compatible with mutmut to ease migration.

## License

TBD
