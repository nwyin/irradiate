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

## Status

What works today:
- Full mutation pipeline (parse → mutate → stats → validate → test → report)
- Pre-warmed pytest worker pool with unix socket IPC
- Table-driven operators (arithmetic, comparison, logical, bitwise, boolean, keyword, unary, string method swaps)
- Procedural operators (numbers, strings, lambdas, assignments)
- Stats-based test selection (only run tests that cover each function)

What's missing:
- Content-addressable cache
- TUI browser
- `--test-command` fallback for non-pytest runners
- Parallel mutation generation (rayon)
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
