# irradiate

Fast mutation testing for Python, written in Rust.

## What mutation testing catches

Your code has `if n < 0`. Your tests pass. But what if it were `if n <= 0`? Would any test fail?

```diff
 def clamp(n, floor=0):
-    if n < floor:
+    if n <= floor:
         return floor
     return n
```

Mutation testing answers this by making small changes to your code — swapping operators, removing arguments, negating conditions — and checking whether your tests notice. A mutation that survives means your tests have a blind spot.

100% line coverage doesn't mean your tests are thorough. Mutation testing tells you where the gaps are.

## Quick start

```bash
pip install irradiate

irradiate run src
```

For a complete walkthrough, see the [Quick Start guide](getting-started/quickstart.md).

## Features

- **Fast** — pre-warmed pytest workers with fork-per-mutant execution. Pytest starts once. Tests run many times.
- **27 mutation operators** — arithmetic, comparison, boolean, string methods, return values, exception types, regex patterns, and more.
- **Incremental** — `--diff main` tests only functions changed since a git ref.
- **Cached** — content-addressed results survive rebases, branch switches, and `touch`.
- **CI-ready** — `--fail-under 80` for gating, GitHub Actions annotations, JSON and HTML reports.
- **Drop-in** — works with any pytest project. `pip install irradiate && irradiate run src`.
