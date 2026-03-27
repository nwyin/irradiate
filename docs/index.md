# irradiate

Fast mutation testing for Python, written in Rust.

## What mutation testing catches

[Mutation testing](https://en.wikipedia.org/wiki/Mutation_testing) works by making small, deliberate changes to your code — like flipping a `<` to `<=`, swapping `True` for `False`, or replacing `+` with `-` — and then running your tests against each change. If a test fails, great: your tests caught the bug. If every test still passes, that's a gap — you have code that can break without any test noticing.

Code coverage tells you which lines ran. Mutation testing tells you which lines are actually *tested*. A function can have 100% line coverage but still have mutants that survive, meaning your tests execute the code without meaningfully checking what it does.

`irradiate` lets you: 

- **Add tests where they matter** — surviving mutants point to exact lines and conditions your tests don't verify, so you know precisely where to write the next test.
- **Tighten weak assertions** — a test that runs the code but only checks `assert result is not None` will let most mutants through. Surviving mutants reveal where you need stricter checks.
- **Remove tests that aren't pulling their weight** — if a test kills zero mutants that other tests don't already catch, it's redundant. Cut it or replace it with something sharper.
- **Find code that isn't reachable** — if every mutant in a block survives and no test even exercises it, that code may be dead. Delete it or question why it exists.

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
