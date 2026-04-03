---
title: Mutation Operators — Cross-Framework Reference
description: Comprehensive catalog of mutation operators across Python mutation testing tools. 38 operator categories in irradiate compared to mutmut, Stryker, and PITest.
---

# Mutation Operators — Cross-Framework Reference

A comprehensive catalog of mutation operators across the mutation testing ecosystem. Compiled as a reference for irradiate's operator coverage and future development.

## irradiate (current)

38 operator categories (27 tree-sitter + 11 regex), ~160+ distinct mutations. Python-specific, operates on tree-sitter CST.

### Operators implemented

| Category              | Operator                | Details                                                                                                                                                  |
| --------------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Binary ops            | `binop_swap`            | 11 pairs: `+`↔`-`, `*`↔`/`, `//`→`/`, `%`→`/`, `**`→`*`, `<<`↔`>>`, `&`↔`\|`, `^`→`&`                                                                    |
| Boolean ops           | `boolop_swap`           | `and`↔`or`                                                                                                                                               |
| Comparison ops        | `compop_swap`           | 10 pairs: `<=`→`<`, `>=`→`>`, `<`→`<=`, `>`→`>=`, `==`↔`!=`, `is`↔`is not`, `in`↔`not in`                                                                |
| Augmented assign      | `augop_swap`            | 11 pairs: `+=`↔`-=`, `*=`↔`/=`, `//=`→`/=`, `%=`→`/=`, `**=`→`*=`, `<<=`↔`>>=`, `&=`↔`\|=`, `^=`→`&=`                                                    |
| Unary ops             | `unary_removal`         | `not x`→`x`, `~x`→`x`                                                                                                                                    |
| Unary sign            | `unary_swap`            | `+x`↔`-x`                                                                                                                                                |
| String methods        | `method_swap`           | 15 pairs: `lower`↔`upper`, `lstrip`↔`rstrip`, `find`↔`rfind`, `ljust`↔`rjust`, `index`↔`rindex`, `removeprefix`↔`removesuffix`, `partition`↔`rpartition` |
| Constants             | `name_swap`             | `True`↔`False`, `deepcopy`→`copy`                                                                                                                        |
| Numbers               | `number_mutation`       | `n`→`n+1` (int and float)                                                                                                                                |
| Constant replacement  | `constant_replacement`  | `n`→`0` (non-zero), `n`→`-n` (positive); int and float                                                                                                   |
| Strings               | `string_mutation`       | `"foo"`→`"XXfooXX"` (skip docstrings, delimiter-containing)                                                                                              |
| String emptying       | `string_emptying`       | `"foo"`→`""` (catches empty-string handling bugs)                                                                                                        |
| Lambdas               | `lambda_mutation`       | body→`None` (or `None`→`0`)                                                                                                                              |
| Assignments           | `assignment_mutation`   | value→`None` (or `None`→`""`)                                                                                                                            |
| Aug-to-plain          | `augassign_to_assign`   | `x += 5`→`x = 5`                                                                                                                                         |
| Arg removal           | `arg_removal`           | Remove each arg individually (skip `len()`, `isinstance()`, generators)                                                                                  |
| Dict kwargs           | `dict_kwarg`            | `dict(foo=1)`→`dict(fooXX=1)`                                                                                                                            |
| Default args          | `default_arg`           | Mutate default parameter values (`None`→`""`, `True`↔`False`, `n`→`n+1`, etc.)                                                                           |
| Return values         | `return_value`          | `return x`→`return None` (or `None`→`""`)                                                                                                                |
| Exception types       | `exception_type`        | `except ValueError:`→`except Exception:` (broaden handler)                                                                                               |
| Match cases           | `match_case_removal`    | Remove each `case` branch (when >1 case)                                                                                                                 |
| Condition negation    | `condition_negation`    | `if cond:`→`if not (cond):`, `while cond:`→`while not (cond):`, `assert cond`→`assert not (cond)`, ternary conditions                                    |
| Condition replacement | `condition_replacement` | `if cond:`→`if True:` / `if False:`, `while cond:`→`while True:` / `while False:`, `elif` (skip if already literal)                                      |
| Statement deletion    | `statement_deletion`    | `x = expr`→`pass`, `return x`→`return None`, `foo()`→`pass`, `raise E`→`pass`                                                                            |
| Keyword swap          | `keyword_swap`          | `break`↔`continue`                                                                                                                                       |
| Loop mutation         | `loop_mutation`         | `for x in items:`→`for x in []:`, `while cond:`→`while False:`                                                                                           |
| Ternary swap          | `ternary_swap`          | `a if cond else b`→`b if cond else a` (skip identical branches)                                                                                          |
| Slice index removal   | `slice_index_removal`   | Remove start/stop/step: `x[1:3]`→`x[:3]`/`x[1:]`, `x[1:5:2]`→`x[:5:2]`/`x[1::2]`/`x[1:5:]`                                                               |

### Skip rules

- Enum subclass methods (`Enum`, `IntEnum`, `StrEnum`, `Flag`, `IntFlag`) — `EnumMeta` metaclass conflicts
- Functions containing `nonlocal` — skipped for trampoline path (source-patch path handles these)
- Descriptor-decorated functions (`@property`, `@classmethod`, `@staticmethod`) — mutated via trampoline
- Other decorated functions (`@lru_cache`, `@app.route`, custom decorators) — mutated via source-patching
- `__getattribute__`, `__setattr__`, `__new__`
- `len()`, `isinstance()` calls (arg_removal skipped — trivially killed, noisy)
- Generator expression / comprehension arguments (arg_removal skipped — invalid syntax)
- Triple-quoted strings (docstrings) — string_mutation and string_emptying skipped
- Strings containing their own delimiter character
- `# pragma: no mutate` lines

---

## Python Ecosystem

### mutmut

The reference implementation irradiate descends from. Uses LibCST for parsing.

Operators largely overlap with irradiate. Notable additions beyond irradiate's current set:

- **String literal case mutations**: `"FooBar"`→`"foobar"`, `"foobar"`→`"FOOBAR"`
- **Dict keyword argument mutation**: `dict(a=1)`→`dict(aXX=1)`
- **`break`→`return`**, **`continue`→`break`** keyword swaps
- **`split`↔`rsplit`** (conditional on maxsplit arg)

### cosmic-ray

Parso-based. Takes a combinatorial approach — generates all pairwise permutations.

| Category       | Approach                                                                                                               |
| -------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Binary ops     | All 132 pairwise permutations of 12 operators                                                                          |
| Comparison ops | All 56 pairwise permutations of 8 operators (context-aware: filters by RHS type)                                       |
| Unary ops      | 5 states (`+`, `-`, `~`, `not`, deletion) — all valid permutations                                                     |
| Booleans       | `True`↔`False`, `and`↔`or`, **condition negation** (`if cond`→`if not cond`, applies to `if`/`while`/`assert`/ternary) |
| Numbers        | `n+1` and `n-1` (both directions)                                                                                      |
| Break/continue | `break`↔`continue`                                                                                                     |
| Exceptions     | Replace exception type with `CosmicRayTestingException`                                                                |
| Decorators     | **Remove each decorator individually**                                                                                 |
| Loops          | **`for x in items`→`for x in []`** (zero iteration)                                                                    |
| Experimental   | Variable replacement (inject random int), variable insertion (inject variable into expression)                         |

### mutpy

Python `ast`-based. Follows classical academic mutation operator naming (AOR, ROR, etc.). Has the richest OOP-specific operators.

| Category        | Operators                                                                                                                                                                                |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Arithmetic      | AOD (unary deletion), AOR (full pairwise replacement)                                                                                                                                    |
| Assignment      | ASR (augmented assignment replacement)                                                                                                                                                   |
| Logical         | COD (remove `not`), COI (insert `not`), LCR (`and`↔`or`), LOD (remove `~`), LOR (bitwise swap)                                                                                           |
| Relational      | ROR (pairwise including `<`→`>` cross-swaps)                                                                                                                                             |
| Constants       | CRP: `5`→`6`, `"hello"`→`"mutpy"`, `"mutpy"`→`"python"`                                                                                                                                  |
| Break/continue  | BCR: `break`↔`continue`                                                                                                                                                                  |
| Decorators      | DDL: remove all decorators                                                                                                                                                               |
| Exceptions      | EHD (handler→`raise`), EXS (handler→`pass`)                                                                                                                                              |
| **Inheritance** | **IHD** (remove shadowing assignment), **IOD** (delete overriding method body), **IOP** (move `super()` call position), **SCD** (delete `super()` call), **SCI** (insert `super()` call) |
| Slicing         | **SIR**: remove lower/upper/step from slices                                                                                                                                             |
| Statements      | **SDL**: delete assignment/return/expression→`pass`                                                                                                                                      |
| Self            | **SVD**: `self.x`→`x` (remove self prefix)                                                                                                                                               |
| Loops           | **OIL** (one iteration: add `break`), **RIL** (reverse: wrap in `reversed()`), **ZIL** (zero iteration: body→`break`)                                                                    |
| Experimental    | CDI (insert `@classmethod`), SDI (insert `@staticmethod`)                                                                                                                                |

---

## JVM Ecosystem

### PIT (pitest) — Java

The most widely-used Java mutation tester. Operates on bytecode. Operators grouped into tiers: OLD_DEFAULTS, DEFAULTS, STRONGER, ALL. Commercial extension Arcmutate adds EXTENDED/EXTREME.

**DEFAULTS group** (recommended production set):

| Operator              | Description                                                                         |
| --------------------- | ----------------------------------------------------------------------------------- |
| CONDITIONALS_BOUNDARY | `<`→`<=`, `<=`→`<`, `>`→`>=`, `>=`→`>`                                              |
| INCREMENTS            | `i++`→`i--` and vice versa                                                          |
| INVERT_NEGS           | `-x`→`x`                                                                            |
| MATH                  | `+`↔`-`, `*`↔`/`, `%`→`*`, `&`↔`\|`, `^`→`&`, `<<`↔`>>`, `>>>`→`<<`                 |
| NEGATE_CONDITIONALS   | `==`↔`!=`, `<=`→`>`, `>=`→`<`, `<`→`>=`, `>`→`<=`                                   |
| VOID_METHOD_CALLS     | Remove void method calls                                                            |
| EMPTY_RETURNS         | `return "foo"`→`return ""`, `return Optional.of(x)`→`return Optional.empty()`, etc. |
| FALSE/TRUE_RETURNS    | `return true`→`return false` and vice versa                                         |
| NULL_RETURNS          | `return obj`→`return null`                                                          |
| PRIMITIVE_RETURNS     | `return 42`→`return 0` (or `0`→`1`)                                                 |

**ALL/Experimental** (additional):

| Operator                          | Description                                                    |
| --------------------------------- | -------------------------------------------------------------- |
| CONSTRUCTOR_CALLS                 | `new Foo()`→`null`                                             |
| INLINE_CONSTS                     | Mutate constants: booleans flip, numbers ±1                    |
| NON_VOID_METHOD_CALLS             | Remove non-void calls, replace return with default             |
| REMOVE_CONDITIONALS               | Replace condition with `true` or `false` (4 variants)          |
| EXPERIMENTAL_AOR                  | Full pairwise arithmetic: `+`→`-`,`*`,`/`,`%` (4 sub-mutators) |
| EXPERIMENTAL_AOD                  | Replace `a + b` with `a` or `b` (operand deletion)             |
| EXPERIMENTAL_ROR                  | Full pairwise relational (5 sub-mutators per operator)         |
| EXPERIMENTAL_CRCR                 | 6 constant replacement strategies: →1, →0, →-1, negate, ±1     |
| EXPERIMENTAL_OBBN                 | Bitwise swap + operand deletion                                |
| EXPERIMENTAL_UOI                  | Insert `++`/`--` around variable uses                          |
| EXPERIMENTAL_SWITCH               | Swap default switch label with first non-default               |
| EXPERIMENTAL_ARGUMENT_PROPAGATION | Replace method call with one of its arguments                  |
| EXPERIMENTAL_NAKED_RECEIVER       | Replace `foo.bar()` with `foo`                                 |
| EXPERIMENTAL_MEMBER_VARIABLE      | Replace field initializer with default                         |

**Arcmutate EXTENDED** (commercial):

Stream/builder-specific: REMOVE_DISTINCT, REMOVE_FILTER, REMOVE_LIMIT, REMOVE_SKIP, REMOVE_SORTED, REMOVE_PREDICATE_NEGATION/AND/OR, CHAINED_CALLS removal, SWAP_PARAMS, SWAP_ALL_MATCH, varargs removal, reactive map swaps (`concatMap`↔`flatMap`↔`switchMap`).

### Major — Java

Compiler plugin (source-level). 9 operator categories following classical naming.

| Operator | Key feature                                                                              |
| -------- | ---------------------------------------------------------------------------------------- |
| AOR      | All 4 alternative replacements per arithmetic op                                         |
| COR      | `&&`→`a`/`b`/`false`/`==`; `\|\|`→`a`/`b`/`true`/`!=`; conditions→`true`/`false`         |
| LOR      | Bitwise swap + operand deletion (`a & b`→`a`, `b`)                                       |
| ROR      | 3-5 replacements per relational op including `true`/`false`                              |
| SOR      | Shift operator pairwise swap + LHS operand                                               |
| ORU      | Unary: `-a`→`a`/`~a`, `~a`→`a`/`-a`, `+a`→`-a`                                           |
| LVR      | Literals: `0`→`1`,`-1`; `c`→`0`,`c±1`,`-c`; `""`↔sentinel; `true`↔`false`                |
| EVR      | Replace expressions with type defaults (`0`/`null`/`true`/`false`)                       |
| **STD**  | **Statement deletion** — remove calls, assignments, increments, returns, break, continue |

### Stryker4s — Scala

AST-level, Scala-specific.

Unique features:

- **18 Scala method expression swaps**: `filter`↔`filterNot`, `exists`↔`forall`, `take`↔`drop`, `takeRight`↔`dropRight`, `takeWhile`↔`dropWhile`, `isEmpty`↔`nonEmpty`, `indexOf`↔`lastIndexOf`, `max`↔`min`, `maxBy`↔`minBy`
- **Regex mutation** via weapon-regex library (20+ patterns: anchor removal, class negation, quantifier removal, lookaround inversion, Unicode property swap)
- Conditional replacement: `if(cond)`→`if(true)`/`if(false)`, `while(cond)`→`while(false)`

---

## JavaScript/TypeScript Ecosystem

### Stryker Mutator — JS/TS

The most comprehensive JS/TS mutation tester.

| Category              | Operators                                                                                                                                                                     |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Arithmetic            | `+`↔`-`, `*`↔`/`, `%`→`*`                                                                                                                                                     |
| Assignment            | 14 pairs including `??=`→`&&=`                                                                                                                                                |
| Equality              | 12 operators: boundary (`<`→`<=`) + negation (`<`→`>=`) + strict (`===`↔`!==`)                                                                                                |
| Logical               | `&&`↔`\|\|`, `??`→`&&`                                                                                                                                                        |
| Boolean               | `true`↔`false`, `!x`→`x`                                                                                                                                                      |
| Unary                 | `+x`↔`-x`                                                                                                                                                                     |
| Update                | `++`↔`--` (pre and post)                                                                                                                                                      |
| Block statement       | **Empty function body**                                                                                                                                                       |
| Conditional           | Replace loop/if condition with `true`/`false`                                                                                                                                 |
| **String**            | `"foo"`→`""`, `""`→`"Stryker was here!"`, template literal→empty                                                                                                              |
| **Array**             | `[1,2,3]`→`[]`, `new Array(1,2,3)`→`new Array()`                                                                                                                              |
| **Object**            | `{foo: 'bar'}`→`{}`                                                                                                                                                           |
| **Optional chaining** | `foo?.bar`→`foo.bar`, `foo?.()`→`foo()`                                                                                                                                       |
| **Method expression** | 20 swaps: `endsWith`↔`startsWith`, `toUpperCase`↔`toLowerCase`, `some`↔`every`, `Math.min`↔`Math.max`, `trim`→`trimEnd`, plus removals (`sort`, `filter`, `reverse`, `slice`) |
| **Regex**             | 26 operators: anchor removal, class negation, `\d`↔`\D`, `\s`↔`\S`, `\w`↔`\W`, quantifier removal, lookaround inversion, Unicode property swap                                |

---

## C#/.NET Ecosystem

### Stryker.NET

Shares the Stryker operator taxonomy with JS/TS-specific additions.

Notable unique operators:

- **LINQ method swaps** (35 pairs): `All`↔`Any`, `First`↔`Last`, `Skip`↔`Take`, `OrderBy`↔`OrderByDescending`, `Union`↔`Intersect`, `Concat`↔`Except`, `Min`↔`Max`, `Count`↔`Sum`, etc.
- **Math method swaps** (23): trig function cross-swaps (`Sin`↔`Cos`↔`Tan` and hyperbolic variants), `Floor`↔`Ceiling`, `Exp`↔`Log`
- **String methods** (16): `StartsWith`↔`EndsWith`, `ToLower`↔`ToUpper`, `TrimStart`↔`TrimEnd`, `PadLeft`↔`PadRight`, `IndexOf`↔`LastIndexOf`, `Trim`→`""`, `Substring`→`""`
- **Initialization emptying**: arrays, lists, dictionaries, objects→empty
- **Removal mutators**: method call removal, return removal, `break`/`continue`/`goto`/`throw` removal, `yield` removal
- **Checked statement removal**: `checked(expr)`→`expr`
- **Null-coalescing**: `a ?? b`→`b ?? a`, `a`, `b`
- **Regex**: 37 operators (most comprehensive regex mutation of any tool)

---

## Rust Ecosystem

### cargo-mutants

Operates on Rust source. Unique approach: **function body replacement** is the primary strategy.

| Category                      | Approach                                                                                                                                                              |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Function body replacement** | Replace entire body with type-appropriate default: `()`/`0`/`1`/`-1`/`true`/`false`/`String::new()`/`"xyzzy"`/`None`/`Ok(default)`/`vec![]`/`Default::default()` etc. |
| Binary ops                    | Multi-replacement: `+`→`-`,`*`; `-`→`+`,`/`; `*`→`+`,`/`; etc.                                                                                                        |
| Assignment ops                | Multi-replacement: `+=`→`-=`,`*=`; etc.                                                                                                                               |
| Comparison                    | `==`↔`!=`, `<`→`==`,`>`; etc.                                                                                                                                         |
| Logical                       | `&&`→`\|\|`,`==`,`!=`; `\|\|`→`&&`,`==`,`!=`                                                                                                                          |
| Unary                         | `-x`→`x`, `!x`→`x` (deletion only)                                                                                                                                    |
| **Match arms**                | Delete non-wildcard arm (when wildcard exists), guard→`true`/`false`                                                                                                  |
| **Struct fields**             | Delete field (only when `..Default::default()` base exists)                                                                                                           |

---

## C/C++ Ecosystem

### Mull (LLVM-based)

Operates on LLVM IR. 44 operators.

Standard set: arithmetic (6), arithmetic assignment (5), comparison (6), boundary (4), bitwise (6), bitwise assignment (5), increment/decrement (4), logical (2), constant assignment (2), function call removal/replacement (2).

---

## PHP Ecosystem

### Infection

The most operator-rich framework overall. 200+ distinct mutation operators.

Unique operators not found elsewhere:

- **Function unwrapping** (49): strip a function call, return its first argument (`array_filter($a, $f)`→`$a`, `strtolower($s)`→`$s`, etc.)
- **Return value mutations** (11): `return $this`→`return null`, `return func()`→`func(); return null`, `return $arr`→`return array_slice($arr, 0, 1, true)`
- **Type cast removal** (6): `(int)$v`→`$v`, `(string)$v`→`$v`, etc.
- **Loop mutations** (6): `foreach($x as ...)`→`foreach([] as ...)`, `while($c)`→`while(false)`, `do{} while($c)`→`do{} while(false)`
- **Exception mutations** (3): remove `throw`, remove `finally`, unwrap `try/finally`
- **Visibility reduction**: `public`→`protected`, `protected`→`private`
- **Catch block dissection**: remove individual exception types from `catch (A|B $e)`
- **Ternary branch swap**: `$x ? $a : $b`→`$x ? $b : $a`
- **Null-safe removal**: `$obj?->method()`→`$obj->method()`
- **Spread mutations**: `[...$arr]`→`$arr`, `[...$arr, 2]`→`[[...$arr][0], 2]`
- **Rounding family** (6): `round`↔`floor`↔`ceil` (all pairwise)
- **BCMath/MBString**: replace arbitrary-precision and multibyte functions with standard equivalents

---

## Cross-Ecosystem Comparison Matrix

Operators that exist in 3+ ecosystems are considered "universal". Operators unique to 1-2 tools are noted.

| Operator Category                 | Python (irradiate)    | Python (others)   | JVM            | JS/TS       | C#/.NET     | Rust          | C/C++ | PHP      |
| --------------------------------- | --------------------- | ----------------- | -------------- | ----------- | ----------- | ------------- | ----- | -------- |
| Arithmetic swap                   | Yes                   | Yes               | Yes            | Yes         | Yes         | Yes           | Yes   | Yes      |
| Comparison boundary               | Yes                   | Yes               | Yes            | Yes         | Yes         | Yes           | Yes   | Yes      |
| Comparison negation               | Yes                   | Yes               | Yes            | Yes         | Yes         | Yes           | Yes   | Yes      |
| Boolean `and`↔`or`                | Yes                   | Yes               | Yes            | Yes         | Yes         | Yes           | --    | Yes      |
| `true`↔`false`                    | Yes                   | Yes               | Yes            | Yes         | Yes         | Yes           | --    | Yes      |
| Negation removal (`not`/`!`)      | Yes                   | Yes               | --             | Yes         | Yes         | Yes           | Yes   | Yes      |
| Augmented assign swap             | Yes                   | Yes               | --             | Yes         | Yes         | Yes           | Yes   | Yes      |
| Aug assign→plain assign           | Yes                   | mutmut            | --             | --          | --          | --            | --    | Yes      |
| Number literal ±1                 | Yes                   | Yes               | Yes            | --          | --          | --            | Yes   | Yes      |
| String mutation                   | Yes (`XX` + emptying) | Yes               | --             | Yes         | Yes         | Yes           | --    | --       |
| Unary `+`↔`-`                     | Yes                   | mutmut            | Yes            | Yes         | Yes         | --            | Yes   | --       |
| Increment `++`↔`--`               | n/a                   | n/a               | Yes            | Yes         | Yes         | --            | Yes   | Yes      |
| Method swaps                      | Yes (string)          | mutmut            | Arcmutate      | Yes         | Yes (LINQ)  | --            | --    | --       |
| Void method removal               | --                    | --                | Yes            | --          | Yes         | --            | Yes   | Yes      |
| Return value replacement          | Yes                   | --                | Yes            | --          | --          | Yes           | Yes   | Yes      |
| Function body→default             | --                    | --                | PIT Extreme    | Yes (block) | Yes (block) | Yes (primary) | --    | --       |
| Statement deletion                | Yes                   | mutpy             | Yes (Major)    | --          | Yes         | --            | --    | --       |
| Decorator removal                 | --                    | cosmic-ray, mutpy | --             | --          | --          | --            | --    | --       |
| Exception handler mutation        | Yes                   | cosmic-ray, mutpy | --             | --          | --          | --            | --    | Yes      |
| Condition→`true`/`false`          | Yes                   | cosmic-ray        | Yes            | Yes         | Yes         | --            | --    | --       |
| Condition negation (insert `not`) | Yes                   | cosmic-ray, mutpy | --             | --          | --          | --            | --    | Yes      |
| Loop zero iteration               | Yes                   | cosmic-ray, mutpy | --             | --          | --          | --            | --    | Yes      |
| Slice index removal               | Yes                   | mutpy             | --             | --          | --          | --            | --    | --       |
| `self.x`→`x`                      | --                    | mutpy             | --             | --          | --          | --            | --    | --       |
| `super()` manipulation            | --                    | mutpy             | --             | --          | --          | --            | --    | --       |
| Regex mutation                    | --                    | --                | --             | Yes (26)    | Yes (37)    | --            | --    | Yes (5)  |
| Optional chaining removal         | --                    | --                | --             | Yes         | --          | --            | --    | Yes      |
| Array/collection emptying         | --                    | --                | --             | Yes         | Yes         | --            | --    | Yes      |
| Object literal emptying           | --                    | --                | --             | Yes         | Yes         | --            | --    | --       |
| Type cast removal                 | --                    | --                | --             | --          | --          | --            | --    | Yes      |
| Function unwrapping               | --                    | --                | --             | --          | --          | --            | --    | Yes (49) |
| Ternary branch swap               | Yes                   | --                | --             | --          | --          | --            | --    | Yes      |
| Match/case removal                | Yes                   | mutmut            | --             | --          | --          | Yes           | --    | Yes      |
| Argument removal                  | Yes                   | mutmut            | Arcmutate      | --          | --          | --            | --    | --       |
| Lambda body mutation              | Yes                   | mutmut            | --             | --          | --          | --            | --    | --       |
| `break`↔`continue`                | Yes                   | mutmut, mutpy, CR | --             | --          | --          | --            | --    | Yes      |
| Visibility reduction              | --                    | --                | --             | --          | --          | --            | --    | Yes      |
| Struct/object field deletion      | --                    | --                | --             | --          | --          | Yes           | --    | --       |
| Match arm guard mutation          | --                    | --                | --             | --          | --          | Yes           | --    | --       |
| Constant→0/negate                 | Yes                   | --                | PIT (CRCR)     | --          | --          | --            | Yes   | --       |
| Operand deletion (`a+b`→`a`)      | --                    | --                | PIT, Major     | --          | --          | --            | --    | --       |
| Argument propagation              | --                    | --                | PIT, Arcmutate | --          | --          | --            | --    | --       |

