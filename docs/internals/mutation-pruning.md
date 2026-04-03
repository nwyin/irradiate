---
title: Mutation Pruning Strategies
description: How irradiate reduces redundant mutants without losing test effectiveness. Coverage-based filtering, operator subsumption, and academic research.
---

# Mutation Pruning

How irradiate minimizes redundant mutant computation without losing test effectiveness.

Each strategy is marked with its status: **active** (implemented), **planned**, or **research**.

## Active strategies

### Coverage-based test selection

Per-function test mapping via the stats plugin. During the stats collection phase,
the harness records which test IDs exercise each trampolined function. When testing
a mutant, only the tests that cover its function are run — not the full suite.

### Skip uncovered functions

Functions with zero test coverage are marked `NoTests` immediately during scheduling,
avoiding the cost of running any tests against them. With `--covered-only`, they are
excluded from results entirely.

### Trampoline / mutation schemata

All mutants for a function are compiled into a single trampolined module. The active
mutant is switched at runtime via the `active_mutant` global — no file I/O, no
recompilation, no process restart per mutant.

### Return statement dedup

`return x` previously generated two identical mutations:
- `return_value`: replace value expression (`x` → `None`)
- `statement_deletion`: replace whole statement (`return x` → `return None`)

Both produce the same output code. We now emit only `return_value`.

**Source**: internal analysis. ~9% mutant reduction on typical code.

### String operator dedup

`"hello"` previously generated two mutations:
- `string_mutation`: `"hello"` → `"XXhelloXX"`
- `string_emptying`: `"hello"` → `""`

Both test whether code is sensitive to string content. We now emit only
`string_emptying` — if code doesn't catch `""`, it won't catch `"XXhelloXX"` either.

**Source**: internal analysis. ~2.5% mutant reduction on typical code.

### Arg removal dedup

`f(a, b)` previously generated both None-replacement and argument removal:
- `f(None, b)` — replace with None (preserves arity)
- `f(b)` — remove entirely (changes arity)

Argument removal usually just crashes with `TypeError`, producing a trivially
killed mutant that wastes test time. We now emit only None-replacement.

**Source**: internal analysis. ~1.9% mutant reduction on typical code.

### Kaminski ROR (Relational Operator Replacement)

Kaminski et al. proved via truth tables that for any relational expression `a op b`,
only 3 mutants are *sufficient* — all others are subsumed (killing one necessarily
kills the subsumed ones). The sufficient set per operator:

| Original | Sufficient mutants |
|----------|-------------------|
| `==`     | `<`, `>`, `False` |
| `!=`     | `<=`, `>=`, `True` |
| `>`      | `>=`, `!=`, `False` |
| `>=`     | `>`, `==`, `True` |
| `<`      | `<=`, `!=`, `False` |
| `<=`     | `<`, `==`, `True` |

Previous behavior generated 1 swap per comparison (e.g., `>` → `>=`). Kaminski tells
us that swap *is* one of the sufficient mutations, but we also need `!=` and `False`
to fully cover the space. The `condition_replacement` operator already generates
`True`/`False`, so we only need to add the second relational replacement.

For `is`/`is not` and `in`/`not in`, which are Python-specific identity/membership
tests (not ordinal), we keep the simple bidirectional swap — Kaminski's ordinal
analysis does not apply.

**Source**: Kaminski, Ammann, Offutt. "Improving Logic-Based Testing" (JSS 2013).
57% reduction on relational operators.

### Arid node filtering

Skip mutations inside code that is never productively tested — mutations that
almost always survive (wasting time) or are trivially killed (adding noise).

**Skipped function names** (`NEVER_MUTATE_FUNCTIONS`):
- `__getattribute__`, `__setattr__`, `__new__` (trampoline-incompatible)
- `__repr__`, `__str__`, `__format__` (display-only, rarely assertion-tested)
- `__hash__` (contractually tied to `__eq__`, mutating alone is misleading)

**Skipped call targets** (`ARID_CALL_TARGETS`):
- `logging.debug`, `logging.info`, `logging.warning`, `logging.error`, `logging.critical`
- `logging.exception`, `logging.log`
- `warnings.warn`

Statement-deletion mutations on these calls are suppressed — removing a log call
almost never causes a test failure.

**Source**: Google "Practical Mutation Testing at Scale" (TSE 2021). Google's
production system reports 82% developer "please fix" rate after arid filtering.

### Python-specific equivalent mutant suppression

Pattern-matching rules that detect mutations known to be equivalent (semantically
identical to the original) or trivially killed (crash immediately, wasting test time).

**Equivalent patterns suppressed**:
- `len(x) > 0` → `len(x) >= 0`: `len()` always returns ≥ 0, so `> 0` and `>= 0`
  differ only at the impossible-to-distinguish-without-context boundary of 0. When
  `len(x) >= 0` is always True, the mutant is equivalent.
- `len(x) == 0` → `len(x) <= 0`: same reasoning — `<= 0` is equivalent to `== 0`
  for len() return values.

**Trivially-killed patterns suppressed**:
- String `a + b` → `a - b`: always raises `TypeError` for strings. The test will
  crash, "killing" the mutant, but this reveals nothing about test quality.

**Source**: EMS (Equivalent Mutant Suppression), Gopinath et al., ISSTA 2024.
Structural pattern-matching rules that detected 4x more equivalents than TCE
(Trivial Compiler Equivalence) at 1/2200th the cost.

### Sampling (`--sample N`)

Randomly sample a subset of mutants for testing. Academic consensus: 5% random
sampling gives 99% R² correlation with the full mutation score (Wong et al. 1995).

**Usage**:
- `--sample 0.1` — test 10% of mutants
- `--sample 100` — test exactly 100 mutants
- `--sample-seed 42` — override the RNG seed (default: 0 for reproducibility)

**Stratification**: mutants are grouped by operator type, and each operator
contributes proportionally to the sample, ensuring no operator class is
completely unrepresented.

**Interaction with other flags**: sampling is applied after `--mutant-names`,
`--diff`, and mutation generation filters. With `--covered-only`, some sampled
mutants may be excluded if they have no test coverage.

**Source**: Wong, Mathur, "Reducing the Cost of Mutation Testing" (JSS 1995).
Zhang et al., "Operator-based and Random Mutant Selection: Better Together"
(ASE 2013).

## Planned strategies

### Mutation levels (`--level 1/2/3`)

Tiered operator selection (Stryker model). Level 1 uses ~5 core operators for speed;
Level 3 uses all operators for thoroughness.

## Research / future

### TCE (Trivial Compiler Equivalence)

Compare `compile(source, optimize=2)` bytecode of original vs mutant. Academic
results show ~28% reduction for C with `-O3`, but **we benchmarked this for Python
and found ~0% detection**. Python's `compile(optimize=2)` only removes `assert` and
`__debug__` — it does not do constant folding (`x*1`), dead code elimination, or
strength reduction. Additionally, default argument mutations produce false positives
(default values live in the enclosing scope, not the function's `co_code`).

**Verdict**: not worth implementing for Python.

### Incremental mode

Hash-based caching of results across runs. Stryker reports 94% cache hit rate.
We have cache infrastructure (`.irradiate/cache/`) that could be extended.

### Weak mutation / equivalence-modulo-states

At the trampoline dispatch point, evaluate both original and mutant expressions.
If they produce the same value for the current test input, skip the full test run.
Could kill most mutants in a single test execution. Highest-ceiling optimization
but requires trampoline changes.

**Source**: AccMut, ISSTA 2017 Distinguished Paper Award.

### Higher-order mutation (SSHOMs)

Combine multiple first-order mutations into one strongly-subsuming higher-order
mutant. 35-45% reduction but requires expensive search.

**Source**: Jia & Harman, IST journal.

### Dynamic subsumption analysis

Build kill matrix after a full run, compute subsumption graph, identify the minimal
non-redundant mutant set. Up to 90% of mutants are redundant.

### Predictive mutation testing

ML/statistical models to predict which mutants will survive. Google's MuRS system
uses "identifier templates" over mutation diffs. MutationBERT uses transformer
models to predict test-mutant kill relationships.

**Sources**: Google TSE 2021, MutationBERT FSE 2023, MuRS FSE 2023, Cerebro TSE 2022.
