# The cursor-position problem

How irradiate finds mutation locations in source code, why it breaks on multi-line expressions, and what a robust long-term solution might look like.

## Background: how mutation positions work

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

## The problem: libcst_native doesn't expose source positions

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

## The cursor-based search mechanism

Since we can't ask a node for its position, we **search** for it. We maintain a monotonically-advancing cursor through the function source. For each expression, we do:

```
codegen_text = codegen_node(expr)
position = source[cursor..].find(codegen_text)
```

The cursor only moves forward, so duplicate tokens (e.g., two `+` operators in `a + b + c`) are always found at their correct respective positions.

This works perfectly for single-line expressions, which is the vast majority of Python code. It breaks for multi-line expressions because `codegen_text != source_text` due to the indentation loss described above.

## Failure modes

### 1. Expression-level: wrong `expr_start`

When `source.find(codegen_text)` returns `None` for a multi-line expression, the fallback is the current cursor position (wrong). All mutations within that expression get wrong byte offsets. `apply_mutation` splices at incorrect positions — eating `return (` prefixes, breaking indentation, producing `IndentationError` or `SyntaxError` in the mutated output.

**Affected patterns**: any multi-line expression — method chains, wrapped returns, multi-line boolean conditions, comprehensions with line breaks.

### 2. Operator-level: wrong sub-positions within expressions

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

### 3. Span-length mismatch: wrong `mutation.end`

Several mutation operators record `original = codegen_text` and compute `end = start + codegen_text.len()`. When `codegen_text` is shorter than the source text (collapsed whitespace), the mutation's `end` is too early. `apply_mutation` leaves the tail of the multi-line expression as an orphaned suffix.

### 4. Replacement mismatch: operators that reconstruct full expressions

`add_arg_removal_mutations` and `add_dict_kwarg_mutations` build a complete replacement expression from codegen pieces. If the original span is multi-line source text but the replacement is single-line codegen text, the splice can strip grouping parentheses that were keeping continuation lines syntactically valid.

## Current mitigations (partial)

As of the `find_expr_span` fix:

- **Expression-level search** uses whitespace-flexible matching: each whitespace run in codegen matches any non-empty whitespace run in source. This correctly finds multi-line expressions and returns the true source span length.
- **Operator keyword search** finds `+`, `and`, `>=`, etc. directly in the source text instead of assuming position from codegen lengths.
- **arg_removal** includes `call.lpar`/`call.rpar` (grouping parens) in replacement text.

These fixes handle the common cases but are inherently fragile — they're patches on a fundamentally position-unaware architecture.

## Remaining edge cases

1. **`add_lambda_mutation_at`** computes body position using `codegen_node(&lam.body).len()` — wrong for multi-line lambda bodies (rare but possible with backslash continuation).

2. **`add_assignment_mutation_at`** sums `codegen_node(target).len()` for all targets to find the value start — wrong if targets span multiple lines.

3. **`add_augassign_to_assign_at`** and other operators that reconstruct statements from codegen pieces — replacements will be single-line even when originals are multi-line. The result is valid Python but has different formatting.

4. **`add_unaryop_mutation_at`** records `expr_source` (multi-line) as original but the replacement is `codegen_node(&unop.expression)` (single-line, missing grouping parens if present on the original).

5. **Nested multi-line expressions** — if a multi-line expression contains another multi-line sub-expression, the ws-flexible match for the inner expression could theoretically match at the wrong position if the codegen text is ambiguous.

## Root cause: using a CST library as a tokenizer

The fundamental issue is an impedance mismatch: we use libcst for structural analysis (what kind of node? what are its children?) but we need positional information (where in the source is this node?). libcst_native doesn't provide the latter, so we reconstruct it through search — and the search breaks when the regenerated text doesn't match the original.

This is not a bug in libcst. libcst is designed for **lossless source transformation** — you modify nodes and codegen the whole tree. We're using it for **position discovery**, which it was never designed for.

## Alternative architectures to consider

### Option A: Patch libcst_native to track byte offsets

Add `start_offset: usize` and `end_offset: usize` to every node during parsing. This is the minimal fix — we'd get exact positions for free and could delete all the cursor/search machinery.

**Pros**: minimal architecture change, all existing mutation operators work as-is.
**Cons**: requires forking and maintaining a patched `libcst_native`. The Rust crate is not actively developed (last meaningful commit was 2023). The patch would need to thread offset tracking through the entire parser, which is non-trivial.

### Option B: Use tree-sitter instead of libcst

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

### Option C: Two-pass approach — tree-sitter for positions, libcst for structure

Use tree-sitter to get byte ranges for all expressions and statements. Use libcst to get structural information (operator types, child relationships). Merge the two by matching nodes.

**Pros**: best of both worlds — accurate positions + rich structural types.
**Cons**: complexity of maintaining two parsers and correlating their outputs. Fragile if the grammars disagree on boundaries.

### Option D: Direct regex/text-pattern mutation (no CST)

Some mutation testing tools (e.g., mutant in Ruby, stryker for JS) use regex patterns and text transformations rather than full parsing. For example: find `and` keyword tokens, replace with `or`. Use a tokenizer to avoid matching inside strings/comments.

**Pros**: trivially position-accurate (regex matches give you byte offsets). Very fast. No parser dependency.
**Cons**: can't do structural mutations (arg removal, ternary swap, match case removal). Limited to token-level swaps. Would lose ~40% of our current mutation operators. Would need a Python tokenizer to avoid false matches in strings/comments.

### Recommendation

**Option B (tree-sitter) is the most promising long-term path.** It eliminates the root cause rather than patching symptoms. The migration could be incremental: add tree-sitter as a parallel parser, migrate one operator at a time, remove libcst when complete. The byte-range-on-every-node property makes position computation trivial and eliminates an entire class of bugs.

The main risk is whether tree-sitter-python's grammar exposes enough structural detail for operators like `arg_removal` and `match_case_removal`. Worth prototyping with those two operators first to validate feasibility before committing to a full migration.
