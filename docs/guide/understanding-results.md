# Understanding Results

After running `irradiate run`, you have results. Here's what they mean and what to do with them.

## Result categories

Every mutant falls into one of these categories:

| Status | Meaning |
|--------|---------|
| **killed** | Your tests caught the mutation — good |
| **survived** | Your tests missed the mutation — investigate |
| **timeout** | Tests ran too long — usually indicates an infinite loop mutant |
| **no tests** | No tests cover this function at all |
| **skipped** | Excluded by `# pragma: no mutate` or config |

## Killed mutants

A killed mutant means irradiate changed something in your code (e.g., `+` to `-`, `and` to `or`) and at least one test failed. Your test suite caught the bug. This is the good outcome.

```
killed: my_module.x_validate__mutmut_2
```

You don't need to do anything for killed mutants.

## Survived mutants

A survived mutant means irradiate changed something and *all your tests still passed*. The mutation wasn't detected. This is a gap in your tests.

```
survived: my_module.x_add__mutmut_3
survived: my_module.x_process__mutmut_1
```

Survived mutants are worth investigating. Start by looking at the diff:

```bash
irradiate show my_module.x_add__mutmut_3
```

Example output:

```diff
--- original
+++ mutant
 def add(a, b):
-    return a + b
+    return a - b
```

For each survivor, decide:

1. **Write a test** — if the mutation represents a real bug your tests should catch
2. **Mark as acceptable** — add `# pragma: no mutate` if the mutation is semantically equivalent in your context
3. **Ignore** — some mutations (e.g., `n` → `n+1` in an index) are equivalent for your actual usage; use judgment

## Timeouts

A timeout means the test suite didn't finish within the time limit. This usually means the mutation created an infinite loop (e.g., `while cond:` mutated to `while True:`). Timeouts count as killed mutants — the mutation disrupted the program.

## No tests

A mutant with no tests means irradiate couldn't find any tests that exercise the mutated function. This is a coverage gap. Consider writing tests for that function, or enable `--covered-only` to skip untested functions entirely.

## Viewing results

```bash
# Show survived mutants (default)
irradiate results

# Show all mutants
irradiate results --all

# Inspect a specific mutant
irradiate show my_module.x_func__mutmut_1
```

## Mutation score

Mutation score = killed / (killed + survived). A higher score means your tests catch more of the mutations irradiate generates.

There's no universal "good" threshold. A score of 80% on a critical payments module is different from 80% on a logging utility. Focus on the mutations that matter for your code's correctness, not the number.

A more useful question: **are there survived mutants in code paths that handle important business logic?** Start there.
