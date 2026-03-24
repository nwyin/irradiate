---
date: 2026-03-21
categories:
  - Mutation Testing
authors:
  - irradiate
description: A practical introduction to mutation testing — what it is, how it works, and why code coverage alone isn't enough.
---

# What Is Mutation Testing (and Why Should You Care)?

Your CI is green. Coverage is at 100%. You ship. Three days later, a customer reports a pricing bug — a discount was applied backwards.

You look at the test. It was there. It ran. It passed. The function was definitely executed.

So why didn't the test catch this?

## The coverage lie

Code coverage tells you which lines ran, not whether your tests would catch a bug in those lines. These are not the same thing.

Here's a function with a latent bug:

```python
def discount_price(price, discount_pct):
    return price * (1 + discount_pct / 100)  # bug: + should be -
```

And a test that covers it:

```python
def test_discount_price():
    result = discount_price(100, 0)
    assert result == 100
```

Coverage: 100%. Bug detection: zero. A zero-percent discount is the one edge case that makes the bug invisible.

This isn't an unusual scenario — it's the default state of most test suites. Tests written alongside the code tend to test the behavior that was implemented, not the behavior that was intended. They mirror the bug.

## Enter mutation testing

Mutation testing works by making small, systematic changes to your source code — *mutations* — and then checking whether your tests catch them. If a mutation goes undetected, you've found a gap in your test suite.

The core loop is simple:

1. Take a function from your codebase
2. Modify one thing (flip an operator, change a constant, swap a comparison)
3. Run the tests against the modified code
4. If the tests pass, the mutant *survived* — your tests didn't notice the change
5. If the tests fail, the mutant was *killed* — your tests are doing their job

A mutant surviving isn't necessarily a bug, but it tells you: "something changed here and nothing cared." That's worth looking at.

## A concrete example

Take this discount function again, written correctly this time:

```python
def discount_price(price, discount_pct):
    return price * (1 - discount_pct / 100)
```

A mutation testing tool would try variations like:

| Mutation | Modified code | What it tests |
|---|---|---|
| Swap `*` to `/` | `price / (1 - discount_pct / 100)` | Tests must check actual price scaling |
| Swap `-` to `+` | `price * (1 + discount_pct / 100)` | Tests must check discount direction |
| Change `100` to `101` | `price * (1 - discount_pct / 101)` | Tests must check exact percentage calculation |

For your tests to kill all three of these mutants, they need to:

- Pass a non-trivial `price` (so the `*` vs `/` swap matters)
- Pass a non-zero `discount_pct` (so the `-` vs `+` swap matters)
- Assert the exact computed value (so off-by-one in the denominator is caught)

If your test only checks `discount_price(100, 0) == 100`, all three mutants survive. If it checks `discount_price(200, 25) == 150.0`, you kill all three.

Mutation testing doesn't tell you what tests to write — it tells you which assumptions your existing tests fail to verify.

## Why it was impractical (until now)

The concept isn't new. It's been in academic literature since the 1970s. So why isn't it standard practice?

Runtime. Traditional mutation testing tools work like this:

1. For each mutant, write a modified source file to disk
2. Fork a new subprocess
3. Re-import all your modules from scratch
4. Re-discover your test suite
5. Run the tests
6. Tear everything down

For a project with 500 mutants and a test suite that takes 2 seconds to run, that's 500 subprocess forks × (2 seconds + startup overhead). In practice, you're looking at 20–40 minutes. For a large codebase, it's hours.

Most teams run it once, see the runtime, and never run it again. Or they never try it at all.

## How irradiate changes this

irradiate approaches the problem differently.

Instead of running your entire test suite against each mutant, it:

1. **Pre-warms a worker pool** — Python processes are started once and kept alive across mutants. No subprocess fork overhead per mutant.

2. **Uses trampoline code injection** — Rather than writing a separate mutated file for each variant, irradiate injects *all* variants of a function into a single file, with a dispatch wrapper that activates the right variant at runtime. Switching mutants is a variable assignment, not a file write and reimport.

3. **Targets tests by coverage** — Before running mutations, irradiate does a single stats pass to determine which tests actually exercise each function. When testing a mutant of `discount_price`, it only runs the tests that call `discount_price` — not your entire test suite.

The result: same mutations, same classification accuracy, a fraction of the time.

## Getting started

If you want to see mutation testing in practice on your own codebase:

```bash
pip install irradiate
cd your-project
irradiate run
```

The [Quick Start guide](../../getting-started/quickstart.md) walks through a full example with annotated output, including how to read the results and prioritize which surviving mutants to address.

Mutation testing works best as a periodic audit rather than a per-commit gate — run it before a major release, after refactoring a critical module, or when you're trying to harden a piece of code that has a history of bugs.

Start with the functions that matter most, get them to zero surviving mutants, and your confidence in those tests will be considerably more justified than a coverage percentage.
