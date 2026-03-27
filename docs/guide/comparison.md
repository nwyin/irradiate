# Comparison

## irradiate vs mutmut

irradiate is heavily inspired by mutmut and shares its trampoline architecture, naming conventions, and config format. The main differences are in execution model and feature set.

|                       | mutmut                                   | irradiate                                                             |
| --------------------- | ---------------------------------------- | --------------------------------------------------------------------- |
| **Execution**         | `pytest.main()` per mutant (~200ms each) | Fork-per-mutant inside pre-warmed workers                             |
| **Parser**            | LibCST (Python, sequential)              | tree-sitter (Rust, parallel via rayon)                                |
| **Operators**         | ~20 categories                           | 38 categories                                                         |
| **Cache**             | mtime-based (breaks on rebase, `touch`)  | Content-addressable (SHA-256)                                         |
| **Orchestration**     | Python multiprocessing                   | Rust + tokio async                                                    |
| **Incremental**       | --                                       | `--diff` with merge-base resolution                                   |
| **Reports**           | Terminal only                            | JSON (Stryker v2), HTML, GitHub Actions annotations                   |
| **Decorator support** | Skip all                                 | @property/@classmethod/@staticmethod handled                          |
| **CI integration**    | Manual                                   | `--fail-under`, inline annotations, step summary                      |
| **Isolation**         | Fork only                                | Warm-session + `--isolate` + `--verify-survivors`                     |
| **Config**            | `[tool.mutmut]`                          | `[tool.irradiate]` (mutmut section accepted with deprecation warning) |

The speedup depends on pytest startup overhead. For projects where pytest takes 200ms+ to start, irradiate is typically 10-50x faster.

## Python ecosystem

### cosmic-ray

Parso-based. Generates all pairwise operator permutations rather than curated swap tables. Supports non-pytest test runners via its session/worker model. Good choice if you need a custom test runner or want exhaustive operator coverage over speed.

### mutpy

Python `ast`-based. Follows academic mutation operator naming (AOR, ROR, etc.). Has object-oriented mutation operators not found elsewhere: inheritance manipulation, `self.x` to `x`, slice index removal. Largely unmaintained.

## Other ecosystems

### cargo-mutants (Rust)

Closest equivalent in design philosophy. Primary strategy is function body replacement — replace the entire body with a type-appropriate default. Simpler to reason about than fine-grained operator swaps but produces fewer total mutations.

### Stryker (JS/TS)

The most feature-rich JS/TS mutation tester. JS-specific operators: optional chaining removal, array/object emptying, regex mutation. Runs as a Node.js process.

### PIT / pitest (Java)

Most widely-used JVM mutation tester. Operates on bytecode, making it language-agnostic across JVM languages. Elaborate operator tier system (DEFAULTS vs STRONGER vs ALL) and a commercial extension (Arcmutate).

### Infection (PHP)

The most operator-dense framework, with 200+ distinct mutation operators. PHP-specific mutations: function unwrapping (strip a call, return its argument), type cast removal, visibility reduction.
