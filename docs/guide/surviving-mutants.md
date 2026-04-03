---
title: What to Do with Surviving Mutants
description: How to interpret and fix surviving mutants from mutation testing. Practical guide to improving Python tests based on mutation results.
---

# Surviving Mutants

When irradiate reports survived mutants, the terminal output groups them by operator category. This page explains what each category tests and how to respond to survivors.

Use `irradiate show <mutant_key>` to see the exact diff for any survivor.

## Arithmetic (`binop_swap`, `augop_swap`, `augassign_to_assign`, `unary_removal`, `unary_swap`)

These mutants flip arithmetic operators: `+` to `-`, `*` to `/`, `+=` to `-=`, or strip unary signs.

```diff
- total = price * quantity
+ total = price / quantity
```

If one of these survives, your tests aren't checking computed results precisely enough. The function ran, returned something, and no assertion caught the wrong value.

**What to do:** Assert on specific outputs. If `calculate_total(10, 3)` should return 30, assert that — don't just check that it returns a number or doesn't raise.

**When to ignore:** Arithmetic in logging, debug strings, or non-critical display formatting where the exact value doesn't affect behavior.

## Comparisons and boundaries (`compop_swap`, `keyword_swap`)

These mutants shift comparison operators: `<` to `<=`, `==` to `!=`, `is` to `is not`, `in` to `not in`.

```diff
- if age >= 18:
+ if age > 18:
```

Survivors here point to a missing boundary test case. The off-by-one didn't break anything because no test exercises the exact boundary value.

**What to do:** Add a test case at the boundary. If the code checks `>= 18`, test with 17, 18, and 19. For `in` / `not in` checks, test with a value that is present and one that isn't.

**When to ignore:** Rarely. Boundary bugs are some of the most common real-world defects. If the boundary genuinely doesn't matter (e.g., an arbitrary batch size threshold), consider whether the comparison itself is unnecessary.

## Booleans and conditions (`boolop_swap`, `condition_negation`, `condition_replacement`)

These mutants flip boolean logic: `and` to `or`, negate conditions with `not`, or replace conditions with `True` / `False`.

```diff
- if user.is_active and user.has_permission:
+ if user.is_active or user.has_permission:
```

Survivors in this group indicate a branch that isn't meaningfully tested. Either your tests only exercise one path, or both paths produce the same observable result in your test setup.

**What to do:** Write tests that exercise both sides of the condition. For `and`/`or` swaps, you need a case where exactly one operand is true — that's where `and` and `or` diverge. For condition negation, test both the true and false branches.

**When to ignore:** Guard clauses that protect against states your test fixtures can't easily reproduce (e.g., checking for a race condition). Use `# pragma: no mutate` and leave a comment explaining why.

## Values and constants (`number_mutation`, `constant_replacement`, `string_emptying`, `name_swap`, `default_arg`)

These mutants change literal values: `n` to `n+1`, `n` to `0`, `"foo"` to `""`, `True` to `False`, or mutate default parameter values.

```diff
- def connect(host, port=5432):
+ def connect(host, port=5433):
```

Survivors here mean the actual value isn't pinned down. The function works with a different constant and nothing breaks.

**What to do:** Assert on the concrete output or side effect that depends on that value. If a default argument matters, write a test that calls the function without that argument and checks the default behavior.

**When to ignore:** Sentinel values, version strings, or constants used only in human-readable output. If changing `0` to `1` in a default doesn't affect correctness, the default might be arbitrary — that's fine.

## Control flow (`statement_deletion`, `return_value`, `loop_mutation`, `match_case_removal`, `ternary_swap`, `lambda_mutation`)

These mutants alter program flow: deleting statements (replacing with `pass`), changing return values, emptying loops (`for x in items` to `for x in []`), removing match/case arms, swapping ternary branches, or mutating lambda bodies.

```diff
  def save(self, record):
-     self.validate(record)
+     pass
      self.db.insert(record)
```

These tend to be the most informative survivors. Statement deletion in particular tells you that a line of code can be removed without any test noticing. Return value and loop survivors similarly indicate code paths whose effects aren't observed by any assertion.

**What to do:** For statement deletion, make sure the deleted statement's side effect is checked — if `validate()` can raise, test that invalid input raises. For return value mutations, assert on what the caller does with the return value. For loop mutations, test with a non-trivial input collection and check that the loop body's effects are visible. For match/case removal, test each branch with a distinct input.

**When to ignore:** Optional side effects like logging calls, metrics emission, or cache warming. If deleting the statement genuinely doesn't affect observable behavior in your domain, suppress it.

## Function signatures (`arg_removal`, `method_swap`, `dict_kwarg`)

These mutants change how functions are called: removing an argument, swapping a method (`lower` to `upper`, `append` to `remove`), or renaming a dict keyword.

```diff
- normalized = name.lower()
+ normalized = name.upper()
```

Survivors here indicate the test calls the function but doesn't assert on the property that the mutated call would change.

**What to do:**

- *Method swap*: Assert on the specific transformation. If you normalize to lowercase, test with mixed-case input and check the output is lowercase.
- *Arg removal*: Test behavior that depends on that argument. If removing an argument doesn't change the output, the argument might be dead code.
- *Dict kwarg*: Test that the keyword argument name matters (i.e., the receiving function uses it).

**When to ignore:** Method calls on mocks where the mock doesn't distinguish between e.g. `append` and `remove`. This can indicate the mock is too lenient — consider tightening it.

## Exception handling (`exception_type`)

This mutant broadens exception handlers: `except ValueError` to `except Exception`.

```diff
- except ValueError:
+ except Exception:
      return default_value
```

The broadened handler silently swallows errors that should propagate. If the mutant survives, no test triggers an exception type that the broader handler would incorrectly catch.

**What to do:** Write a test that triggers a different exception type (e.g., `TypeError`) in the same code path and verify it propagates rather than being caught. The point is to test that the handler is specific enough.

**When to ignore:** Catch-all handlers that are intentionally broad, like top-level error boundaries in CLI entry points. Suppress with `# pragma: no mutate`.

## Assignments (`assignment_mutation`)

This mutant replaces the right-hand side of assignments with `None` (or `None` to `""`).

```diff
- self.connection = db.connect(url)
+ self.connection = None
```

If this survives, the assigned value isn't read back by any test — or the test doesn't distinguish between the real value and `None`.

**What to do:** Assert on behavior that depends on the assigned value. If `self.connection` is used later, a test that exercises that usage should catch this. If no test fails, the assignment's result might be unused.

**When to ignore:** Assignments to variables consumed only by code paths not under test (e.g., debug state). Consider whether the assignment is actually dead code.

## Slicing (`slice_index_removal`)

This mutant removes start, stop, or step indices from slices.

```diff
- items = data[1:10]
+ items = data[:10]
```

Survivors here usually mean the test input is too small for the slice boundaries to matter. The full slice and the truncated one produce the same result on your test data.

**What to do:** Use test inputs large enough that removing a slice bound changes the result. If `data[1:10]` should skip the first element, test with data where the first element is distinct and assert it's absent.

**When to ignore:** Slices used for display truncation or similar non-critical formatting.

## Regex patterns (`regex_*`)

Eleven operators target regex patterns: removing anchors (`^`, `$`), negating character classes, flipping shorthand classes (`\d` to `\D`), removing quantifiers, and more.

```diff
- pattern = re.compile(r"^\d{3}-\d{4}$")
+ pattern = re.compile(r"\d{3}-\d{4}$")
```

Survivors in regex operators mean your test inputs are accepted by both the original and mutated pattern. Regex tests tend to only check that valid input matches, but mutation testing reveals whether you also test that invalid input is rejected.

**What to do:**

- *Anchor removal*: Test with input that has extra content before/after the expected match.
- *Charclass/shorthand negation*: Test with input where flipping the class (e.g., digits to non-digits) would fail.
- *Quantifier removal/change*: Test with input that exercises the repetition — too few or too many repetitions of the quantified element.
- *Alternation removal*: Test each alternative in the pattern.
- *Lookaround negation*: Test input that should match the lookahead/lookbehind and input that shouldn't.

**General principle:** For every regex, test at least one input that should match and one that should *not* match. Anchor and quantifier survivors almost always indicate missing negative test cases.

**When to ignore:** Regexes used for loose extraction where false positives are acceptable (e.g., log parsing).

---

## When to suppress vs. when to fix

Not every survivor warrants a new test. Use this as a rough guide:

| Action | When |
|--------|------|
| **Write a test** | The mutation represents a bug that could happen in production and your tests should catch it |
| **Suppress** (`# pragma: no mutate`) | The mutation is semantically equivalent in your context, or the code is intentionally imprecise |
| **Do nothing** | Low-risk code (logging, debug output) where the cost of a test exceeds the value |

If you find yourself suppressing many mutants in the same function, that can signal the function is doing too many things or is hard to test — consider refactoring.
