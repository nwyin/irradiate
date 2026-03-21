# Comparison

## irradiate vs mutmut

mutmut is the direct ancestor of irradiate. They share the same trampoline architecture, mutation operators, naming conventions, and output format. The difference is in how they run tests.

| | mutmut | irradiate |
|---|---|---|
| **Startup cost** | `pytest.main()` per mutant (~200ms each) | Pre-warmed worker pool — pytest starts once, runs many |
| **Cache** | mtime-based (breaks on rebase, `touch`, branch switch) | Content-addressable (SHA-256 of function body + tests + operator) |
| **Orchestration** | Python multiprocessing | Rust + tokio async (no GIL, native signal/timeout handling) |
| **Mutation dispatch** | `os.environ` lookup per call (syscall) | Module global lookup (dict access, no syscall) |
| **Mutation generation** | Sequential Python (LibCST) | Parallel Rust (libcst crate + rayon) |
| **Result I/O** | JSON write per mutant | Batched writes |
| **Isolation** | Fork per mutant only | Default warm-session + `--isolate` flag for full subprocess isolation |
| **State leakage** | None (fresh process per mutant) | Module snapshot/restore between runs, session-fixture-aware recycling, `--verify-survivors` safety net |
| **Worker health** | — | Memory monitoring, automatic respawn, configurable recycling |
| **Test selection** | Coverage-based | Coverage-based + duration-aware scheduling |

The practical speedup depends on your test suite's startup overhead. For projects where `pytest` takes 200ms+ to start, irradiate is typically 10–50× faster.

## Python ecosystem

### cosmic-ray

Parso-based. Takes a combinatorial approach — generates all pairwise operator permutations rather than irradiate's curated swap tables. Also supports the widest range of test runners via its session/worker model. Good choice if you need a non-pytest test runner or want exhaustive operator coverage over practical speed.

### mutpy

Python `ast`-based. Follows academic mutation operator naming (AOR, ROR, etc.). Has the richest object-oriented mutation operators: inheritance manipulation (`super()` moves, method deletion), `self.x`→`x`, slice index removal. Largely unmaintained.

## Other ecosystems

### cargo-mutants (Rust)

The closest equivalent to irradiate in design philosophy: fast, practical, built as a native tool. Primary strategy is **function body replacement** — replace the entire function body with a type-appropriate default. Simpler to reason about than fine-grained operator swaps but produces fewer total mutations.

### Stryker (JS/TS)

The most comprehensive JS/TS mutation tester. Has unique operators specific to JS: optional chaining removal (`foo?.bar`→`foo.bar`), array/object emptying, regex mutation. Runs as a Node.js process; handles Jest, Vitest, and other JS test runners natively.

### PIT / pitest (Java)

The most widely-used JVM mutation tester. Operates on bytecode rather than source, which makes it language-agnostic across JVM languages. Has the most elaborate operator tier system (DEFAULTS vs STRONGER vs ALL) and a commercial extension (Arcmutate) with LINQ/stream-specific operators.

### Stryker.NET (C#)

Shares Stryker's taxonomy with C#-specific additions: 35 LINQ method swaps, 23 Math method swaps, 37 regex operators (most comprehensive regex mutation of any tool).

### Infection (PHP)

The most operator-rich framework overall — 200+ distinct mutation operators. Has PHP-specific mutations not found elsewhere: function unwrapping (strip a call, return its argument), type cast removal, null-safe removal.

## Positioning

irradiate's niche is: **same mutation operators as mutmut, significantly faster execution, without changing how you write tests or configure your project**. You run `irradiate run` instead of `mutmut run`. The output format is compatible. The config section is compatible (`[tool.mutmut]` is accepted with a deprecation warning).

If you're already using mutmut, irradiate is a drop-in replacement that should be faster. If you're not using mutation testing yet and you use pytest, irradiate is the practical starting point.
