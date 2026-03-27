# Understanding Results

## Result categories

Every mutant gets one of these outcomes:

| Status       | Meaning                                              |
| ------------ | ---------------------------------------------------- |
| **Killed**   | Tests caught the mutation                            |
| **Survived** | Tests missed the mutation — investigate              |
| **Timeout**  | Tests ran too long (usually an infinite loop mutant) |
| **No tests** | No tests cover this function                         |
| **Error**    | Worker crashed (segfault, import error, etc.)        |

## Killed mutants

A killed mutant means irradiate changed something (`+` to `-`, `and` to `or`, etc.) and at least one test failed. No action needed.

## Survived mutants

A survived mutant means irradiate changed something and all tests still passed. This is a gap.

```bash
irradiate show mylib.x_validate__irradiate_1
```

```diff
--- original
+++ mutant
 def validate(value):
-    if value > 0:
+    if value >= 0:
         return True
```

For each survivor, decide what to do. If the mutation represents a real bug your tests should catch, write a test. If the mutation is semantically equivalent in your context, add `# pragma: no mutate` to suppress it. Some survivors (e.g., `n` to `n+1` in a non-critical index) may not warrant a test at all.

See [Surviving Mutants](surviving-mutants.md) for detailed guidance on each operator category.

## Timeouts

A timeout means the mutation created a non-terminating condition, like `while cond:` mutated to `while True:`. Timeouts count as killed because the mutation broke the program.

## Mutation score

Score = killed / (killed + survived). Higher is better.

There's no universal threshold. Focus on survivors in code paths that handle important logic. A survived `+` to `-` in a pricing function is more concerning than one in a logging format string.

## Viewing results

```bash
# Survived mutants only
irradiate results

# All mutants with status
irradiate results --all

# JSON output
irradiate results --json

# Inspect one mutant
irradiate show mylib.x_func__irradiate_1
```

## Reports

```bash
# Stryker-compatible JSON
irradiate run --report json

# Self-contained HTML (opens in browser)
irradiate run --report html
```

In GitHub Actions, survived mutants appear as inline warning annotations on the PR diff.
