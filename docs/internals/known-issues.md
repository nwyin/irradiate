# Known Issues

Known edge cases and limitations in irradiate's mutation engine. These are documented problems with analysis and proposed solutions — not bugs to simply avoid, but constraints to understand when contributing or debugging unexpected behavior.

---

## The `super()` `__class__` Cell Problem

### Background

Python 3's `super()` without arguments (PEP 3135) relies on a compiler-injected
`__class__` cell variable. When the compiler sees `super` referenced inside a
class body method, it:

1. Adds `__class__` to the method's `co_freevars` (closure variables)
2. Generates `LOAD_DEREF __class__` + `LOAD_SUPER_ATTR` bytecode
3. After the class object is created, populates the cell with a reference to it

A function compiled at **module level** gets fundamentally different bytecode:
`LOAD_GLOBAL super` + regular `CALL`. You cannot fix this by patching
`co_freevars` after the fact — the bytecode instructions are wrong.

### How irradiate triggers this

irradiate's trampoline architecture extracts class methods to module level:

```python
# Original (inside class body — __class__ cell works):
class Markup(str):
    def __add__(self, value):
        return super().__add__(self.escape(value))

# After irradiate codegen (module level — __class__ cell missing):
def xǁMarkupǁ__add____mutmut_orig(self, value):
    return super().__add__(self.escape(value))  # RuntimeError!
```

The wrapper stays inside the class body, but the mangled orig/variants at module
level lose the `__class__` cell. Any method using `super()` fails with:
`RuntimeError: super(): __class__ cell not found`

Found via markupsafe vendor testing — `Markup.__add__`, `__radd__`, `__mod__`,
`format_map`, etc. all use `super()`.

### How other tools handle this

#### mutmut — keep everything inside the class body

mutmut keeps orig, variants, and lookup dict **inside the class body**
(`file_mutation.py:220-238`). The mangled names become class attributes rather
than module globals. Since everything is compiled in the class context, the
`__class__` cell is always present.

mutmut has zero code, tests, or documentation related to `super()` / `__class__`
because their architecture avoids the problem entirely.

#### cosmic-ray, MutPy — in-place single-mutation edits

These tools apply one mutation at a time to the original source, without moving
code. The method definition stays in the class body. No `super()` problem.

#### mutatest — bytecode-only modification

Modifies compiled `.pyc` files in `__pycache__`. Compilation happens in the
correct context. No `super()` problem.

### Possible solutions for irradiate

#### A. Keep mangled code inside the class body (mutmut's approach)

Instead of extracting `module_code` to module level for class methods, emit
orig, variants, and lookup dict inside the class body (indented).

- **Pros**: Completely eliminates the problem. No detection needed. Simplest
  correctness guarantee.
- **Cons**: Class bodies become large. Mangled names become class attributes
  rather than module globals. May interact with metaclasses or
  `__init_subclass__` that introspect class attributes.

#### B. Rewrite `super()` to explicit `super(ClassName, self)`

Scan each method's source for `super()` calls. When found, rewrite to
`super(ClassName, self)` before extracting to module level.

- **Pros**: Minimal architectural change. Only affects methods that use `super()`.
  Preserves module-level extraction for everything else.
- **Cons**: Must handle edge cases: `super` aliased, `super` in nested
  functions/lambdas, classmethods (`cls` instead of `self`). Subtly changes
  behavior in exotic multiple-inheritance scenarios (though in practice
  identical for direct subclass methods).
- **Implementation**: Simple regex/text replacement in Rust codegen:
  `super()` → `super(ClassName, self)`. Covers 99% of real-world code.

#### C. Closure factory wrapper

Wrap extracted functions in a closure that provides `__class__`:

```python
def _make_method(cls):
    __class__ = cls
    def xǁChildǁgreet__mutmut_orig(self):
        return super().greet()
    return xǁChildǁgreet__mutmut_orig
xǁChildǁgreet__mutmut_orig = _make_method(Child)
```

- **Pros**: Preserves original `super()` semantics perfectly. Works with all
  inheritance patterns.
- **Cons**: Adds indirection. Class must be defined before the factory call
  (ordering constraint). One factory per method. Runtime overhead at import.

#### D. Hybrid — detect `super()` and choose strategy per-method

If a method (or its mutant variants) uses `super()`, keep it in the class body.
Otherwise, extract to module level.

- **Pros**: Best of both worlds — most methods get module-level extraction,
  only `super()`-using methods stay in class.
- **Cons**: Two code paths to maintain. Detection must be reliable.

### Recommendation

**Approach A (keep mangled code inside the class body)** — matching mutmut's
proven strategy.

Approach B (rewrite `super()`) was initially tempting as a minimal change, but
it has real edge-case risks: `super` aliased to a variable, nested in
comprehensions/lambdas, classmethods with `cls` instead of `self`, and subtle
behavioral differences in multiple-inheritance diamonds where explicit
`super(ClassName, self)` is not identical to implicit `super()`.

Approach A avoids the problem entirely with no detection or rewriting needed.
The downsides are cosmetic:

- **Class attribute pollution**: mangled names like `xǁClassǁmethod__irradiate_orig`
  appear in `dir(cls)` / `vars(cls)`. Unlikely to collide with anything due to
  the `xǁ` prefix. Metaclasses that introspect `__dict__` would see them, but
  this is the same tradeoff mutmut makes and hasn't been a problem in practice.
- **Larger class bodies**: more code inside the class. No runtime impact.

mutmut has been shipping this approach for years with real users. It works.

### What changes in codegen

Currently (`src/codegen.rs` and `src/trampoline.rs`):
- Wrapper stays inside the class body (indented) ✓
- Orig, variants, and lookup dict are extracted to module level ✗

After the fix:
- Wrapper stays inside the class body ✓
- Orig, variants, and lookup dict also stay inside the class body ✓
- Top-level functions continue to use module-level placement (no change)

### References

- [PEP 3135 — New Super](https://peps.python.org/pep-3135/)
- [CPython issue #29944 — super() fails in type()-constructed classes](https://bugs.python.org/issue29944)
- mutmut source: `vendor/mutmut/src/mutmut/file_mutation.py` lines 220-238

---

## The Cursor-Position Problem

How irradiate finds mutation locations in source code, why it breaks on multi-line expressions, and what a robust long-term solution might look like.

### Background: how mutation positions work

irradiate uses [libcst_native](https://crates.io/crates/libcst_native) (the Rust port of Python's LibCST) to parse Python source into a concrete syntax tree. We walk the CST to find mutation opportunities, but we don't modify the CST — we produce **text substitutions** with byte offsets into the original source:

```rust
struct Mutation {
    start: usize,   // byte offset in function source
    end: usize,     // byte offset one past original
    original: String,
    replacement: String,
    operator: &'static str,
}
```

`apply_mutation` is just string splicing: `source[..start] + replacement + source[end..]`.

The critical question is: **how do we compute `start` and `end`?**

### The problem: libcst_native doesn't expose source positions

libcst (both the Python and Rust versions) is a *concrete* syntax tree — it preserves all whitespace, comments, and formatting. But the Rust crate (`libcst_native`) does not expose byte offsets or source spans on its nodes. There's no `.start_pos` or `.end_pos` field. You get a tree of nodes with whitespace baked into their fields, but no way to ask "where in the source did this node come from?"

The only way to get text out of a node is `Codegen`:

```rust
fn codegen_node(node: &impl Codegen) -> String {
    let mut state = CodegenState::default();
    node.codegen(&mut state);
    state.tokens
}
```

This regenerates source text from the node. For single-line expressions, this is identical to the original source. For multi-line expressions, it is **not** — because `CodegenState::default()` has an empty indent stack, so `INDENT` tokens resolve to empty strings. The result preserves newlines but loses indentation:

```python
# Original source:
return (
    s.replace("&", "&amp;")
    .replace(">", "&gt;")
)

# codegen_node() output:
return (
s.replace("&", "&amp;")
.replace(">", "&gt;")
)
```

### The cursor-based search mechanism

Since we can't ask a node for its position, we **search** for it. We maintain a monotonically-advancing cursor through the function source. For each expression, we do:

```
codegen_text = codegen_node(expr)
position = source[cursor..].find(codegen_text)
```

The cursor only moves forward, so duplicate tokens (e.g., two `+` operators in `a + b + c`) are always found at their correct respective positions.

This works perfectly for single-line expressions, which is the vast majority of Python code. It breaks for multi-line expressions because `codegen_text != source_text` due to the indentation loss described above.

### Failure modes

#### 1. Expression-level: wrong `expr_start`

When `source.find(codegen_text)` returns `None` for a multi-line expression, the fallback is the current cursor position (wrong). All mutations within that expression get wrong byte offsets. `apply_mutation` splices at incorrect positions — eating `return (` prefixes, breaking indentation, producing `IndentationError` or `SyntaxError` in the mutated output.

**Affected patterns**: any multi-line expression — method chains, wrapped returns, multi-line boolean conditions, comprehensions with line breaks.

#### 2. Operator-level: wrong sub-positions within expressions

Even after finding the correct expression start, sub-positions within a multi-line expression are wrong. The code uses:

```rust
collect_expr_mutations(&binop.left, source, &mut local, mutations, ignored);
let op_text = codegen_node(&binop.operator);
add_binop_mutation_at(&binop.operator, &op_text, local, mutations);
local += op_text.len();  // <-- codegen length, not source length!
```

The operator's codegen text includes whitespace fields from the CST node. With `CodegenState::default()`, a `BooleanOp::And` that sits on a continuation line produces `"\nand "` (6 bytes) but the source has `"\n            and "` (17 bytes). The sub-cursor advances by 6, not 17, so it drifts out of sync.

This produces mutations where the replacement is inserted at the wrong position without removing the original operator:

```python
# Source:
if (
    default_map is None
    and info_name is not None   # <-- target
):

# Expected mutation:
    or info_name is not None

# Actual (broken) mutation:
    or and info_name is not None
#   ^^^ inserted at wrong position, "and" not removed
```

#### 3. Span-length mismatch: wrong `mutation.end`

Several mutation operators record `original = codegen_text` and compute `end = start + codegen_text.len()`. When `codegen_text` is shorter than the source text (collapsed whitespace), the mutation's `end` is too early. `apply_mutation` leaves the tail of the multi-line expression as an orphaned suffix.

#### 4. Replacement mismatch: operators that reconstruct full expressions

`add_arg_removal_mutations` and `add_dict_kwarg_mutations` build a complete replacement expression from codegen pieces. If the original span is multi-line source text but the replacement is single-line codegen text, the splice can strip grouping parentheses that were keeping continuation lines syntactically valid.

### Current mitigations (partial)

As of the `find_expr_span` fix:

- **Expression-level search** uses whitespace-flexible matching: each whitespace run in codegen matches any non-empty whitespace run in source. This correctly finds multi-line expressions and returns the true source span length.
- **Operator keyword search** finds `+`, `and`, `>=`, etc. directly in the source text instead of assuming position from codegen lengths.
- **arg_removal** includes `call.lpar`/`call.rpar` (grouping parens) in replacement text.

These fixes handle the common cases but are inherently fragile — they're patches on a fundamentally position-unaware architecture.

### Remaining edge cases

1. **`add_lambda_mutation_at`** computes body position using `codegen_node(&lam.body).len()` — wrong for multi-line lambda bodies (rare but possible with backslash continuation).

2. **`add_assignment_mutation_at`** sums `codegen_node(target).len()` for all targets to find the value start — wrong if targets span multiple lines.

3. **`add_augassign_to_assign_at`** and other operators that reconstruct statements from codegen pieces — replacements will be single-line even when originals are multi-line. The result is valid Python but has different formatting.

4. **`add_unaryop_mutation_at`** records `expr_source` (multi-line) as original but the replacement is `codegen_node(&unop.expression)` (single-line, missing grouping parens if present on the original).

5. **Nested multi-line expressions** — if a multi-line expression contains another multi-line sub-expression, the ws-flexible match for the inner expression could theoretically match at the wrong position if the codegen text is ambiguous.

### Root cause: using a CST library as a tokenizer

The fundamental issue is an impedance mismatch: we use libcst for structural analysis (what kind of node? what are its children?) but we need positional information (where in the source is this node?). libcst_native doesn't provide the latter, so we reconstruct it through search — and the search breaks when the regenerated text doesn't match the original.

This is not a bug in libcst. libcst is designed for **lossless source transformation** — you modify nodes and codegen the whole tree. We're using it for **position discovery**, which it was never designed for.

### Alternative architectures to consider

#### Option A: Patch libcst_native to track byte offsets

Add `start_offset: usize` and `end_offset: usize` to every node during parsing. This is the minimal fix — we'd get exact positions for free and could delete all the cursor/search machinery.

**Pros**: minimal architecture change, all existing mutation operators work as-is.
**Cons**: requires forking and maintaining a patched `libcst_native`. The Rust crate is not actively developed (last meaningful commit was 2023). The patch would need to thread offset tracking through the entire parser, which is non-trivial.

#### Option B: Use tree-sitter instead of libcst

[tree-sitter-python](https://github.com/tree-sitter/tree-sitter-python) provides exact byte ranges on every node. It's actively maintained, battle-tested (used by GitHub, Neovim, Helix, Zed), and has a mature Rust API.

```rust
// tree-sitter gives us this for free:
let node = cursor.node();
let start = node.start_byte();
let end = node.end_byte();
let text = &source[start..end];
```

**Pros**: eliminates the entire cursor-search mechanism. Every node has exact positions. Incremental parsing support for future use. Active maintenance. Would also simplify the codegen module since we'd always have access to source text.

**Cons**: tree-sitter produces an AST, not a CST — it may not preserve all whitespace details in its node structure (though the byte ranges into the original source give us the exact text anyway). Would require rewriting the mutation walker to use tree-sitter's node types instead of libcst's. The tree-sitter grammar for Python may not expose all the structural detail we rely on (e.g., individual `lpar`/`rpar`, whitespace fields on operators).

#### Option C: Use Astral/Ruff's Python parser and AST

Astral's parser stack (the Rust crates behind Ruff, not `uv` directly) parses Python into a typed AST where nodes carry `TextRange` source spans, and the parse result also includes a token stream with ranges. `TextSize` is defined as a UTF-8 byte offset, so these ranges map directly to our current `start: usize` / `end: usize` mutation model.

```rust
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

let parsed = parse_module(source)?;
let expr = /* walk AST */;
let start = expr.start().to_usize();
let end = expr.end().to_usize();
let text = &source[start..end];
```

This would eliminate the expression-level cursor search entirely. For token-level mutations (`and` -> `or`, `+` -> `-`), we would still need to locate the operator token within the containing expression span, but the parser exposes a `Tokens` collection with per-token ranges, so that search can be constrained to the exact node span rather than the whole function body.

**Pros**: exact byte ranges on AST nodes. Token ranges are also available, which helps for operator swaps and punctuation-sensitive mutations. The typed Python AST is richer and more semantically convenient than tree-sitter's generic syntax nodes.

**Cons**: these crates are effectively internal Ruff implementation crates, not a stable published library API (`version = "0.0.0"`, `publish = false`). Depending on them directly by git revision would be brittle. In practice, the cleaner adoption path would likely be to vendor or fork the parser into irradiate and maintain it ourselves. That is operationally reasonable, but it changes the ownership model: we would need to track new Python syntax, decide our supported Python-version matrix explicitly, and periodically sync upstream parser fixes when they matter to us.

#### Option D: Two-pass approach — tree-sitter for positions, libcst for structure

Use tree-sitter to get byte ranges for all expressions and statements. Use libcst to get structural information (operator types, child relationships). Merge the two by matching nodes.

**Pros**: best of both worlds — accurate positions + rich structural types.
**Cons**: complexity of maintaining two parsers and correlating their outputs. Fragile if the grammars disagree on boundaries.

#### Option E: Direct regex/text-pattern mutation (no CST)

Some mutation testing tools (e.g., mutant in Ruby, stryker for JS) use regex patterns and text transformations rather than full parsing. For example: find `and` keyword tokens, replace with `or`. Use a tokenizer to avoid matching inside strings/comments.

**Pros**: trivially position-accurate (regex matches give you byte offsets). Very fast. No parser dependency.
**Cons**: can't do structural mutations (arg removal, ternary swap, match case removal). Limited to token-level swaps. Would lose ~40% of our current mutation operators. Would need a Python tokenizer to avoid false matches in strings/comments.

### Recommendation and decision

**Option B (tree-sitter) and Option C (Astral/Ruff parser) are the most credible long-term paths.** Both eliminate the root cause by giving us exact source ranges on parsed nodes instead of forcing text search over `libcst` codegen output.

**Decision: Choose tree-sitter.**

Ruff's parser is attractive on technical grounds: typed Python AST, exact byte spans, and token ranges. If irradiate were an internal tool with no packaging constraints, it would be a strong contender. But irradiate is intended to be a public tool that should eventually be easy to install from PyPI, may grow Python bindings, and may also want to expose a reusable Rust core. In that context, tree-sitter is the better fit.

The reason is not that Ruff's parser lacks capability; it is that tree-sitter is designed to be embedded as a stable parser dependency, while Ruff's parser crates are internal implementation crates. Choosing Ruff would effectively mean adopting and maintaining a parser subsystem inside irradiate. That is feasible, but it is not free, and it widens the scope of the project in a way that is orthogonal to mutation testing itself.

tree-sitter gives us the key property we actually need — exact byte ranges on syntax nodes — without forcing us into parser ownership. It has a mature Rust API, broad ecosystem adoption, and a cleaner dependency story for binary distribution, PyPI packaging, and any future crate consumers. For irradiate as a product, that makes it the better long-term choice.
