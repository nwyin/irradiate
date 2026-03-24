# Regex Mutation Operators — Implementation Plan

**Status**: In progress
**Date**: 2026-03-24

## Motivation

No Python mutation tester currently supports regex mutation. Stryker JS has 26 regex operators, Stryker.NET has 37+, and Infection PHP has 5. Regular expressions are a significant source of bugs in Python code — anchors missing, character classes inverted, quantifiers wrong — and test suites that exercise regex-heavy code often fail to detect subtle pattern errors.

irradiate already has 27 operator categories with ~160+ distinct mutations. Adding regex mutation fills the biggest remaining gap in the cross-framework comparison matrix (see `docs/internals/mutation-operators.md`).

---

## Detection: Finding Regex Patterns in Python Source

### Call-site detection via tree-sitter

Regex patterns in Python appear as string arguments to `re` module functions. The tree-sitter CST gives us enough structure to identify these call sites without type inference.

**Target call sites** (first positional argument is the pattern):

```python
re.compile(pattern, flags=0)
re.match(pattern, string)
re.search(pattern, string)
re.findall(pattern, string)
re.finditer(pattern, string)
re.fullmatch(pattern, string)
re.sub(pattern, repl, string)
re.subn(pattern, repl, string)
re.split(pattern, string)
```

**Tree-sitter node structure** for `re.compile(r"^\d+$")`:

```
call
├── function: attribute
│   ├── object: identifier "re"
│   └── attribute: identifier "compile"
└── arguments: argument_list
    └── string: r"^\d+$"
```

**Detection algorithm** (Rust pseudocode):

```rust
/// Recognized `re` module function names whose first positional arg is a pattern.
const RE_PATTERN_FUNCTIONS: &[&str] = &[
    "compile", "match", "search", "findall", "finditer",
    "fullmatch", "sub", "subn", "split",
];

/// Check if a `call` node is a `re.<func>(pattern, ...)` call and return
/// the first positional string argument node if so.
fn detect_regex_call<'a>(
    node: Node<'a>,
    source: &str,
) -> Option<Node<'a>> {
    // Must be a call node.
    if node.kind() != "call" {
        return None;
    }

    let function_node = node.child_by_field_name("function")?;

    // Handle `re.compile(...)` form (attribute access).
    if function_node.kind() == "attribute" {
        let object = function_node.child_by_field_name("object")?;
        let attribute = function_node.child_by_field_name("attribute")?;
        let obj_text = node_text(source, object);
        let attr_text = node_text(source, attribute);

        // Accept `re.<func>` and `regex.<func>` (both stdlib and third-party).
        if obj_text != "re" && obj_text != "regex" {
            return None;
        }
        if !RE_PATTERN_FUNCTIONS.contains(&attr_text) {
            return None;
        }
    } else {
        // Handle bare `compile(...)` — but this is too ambiguous.
        // v1: only match `re.<func>` attribute calls.
        return None;
    }

    // Extract the first positional argument.
    let arguments = node.child_by_field_name("arguments")?;
    let first_arg = first_positional_string_arg(arguments, source)?;

    // Must be a string literal (not a variable, f-string, or concatenation).
    if first_arg.kind() != "string" {
        return None;
    }

    Some(first_arg)
}

/// Return the first positional (non-keyword) argument that is a string node.
fn first_positional_string_arg<'a>(
    arguments: Node<'a>,
    source: &str,
) -> Option<Node<'a>> {
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        // Skip keyword arguments.
        if child.kind() == "keyword_argument" {
            continue;
        }
        // Skip *args and **kwargs.
        let text = node_text(source, child);
        if text.starts_with('*') {
            continue;
        }
        // Return the first positional argument if it's a string.
        if child.kind() == "string" {
            return Some(child);
        }
        // If first positional is not a string, bail.
        return None;
    }
    None
}
```

### What we do NOT attempt to detect (v1)

These require dataflow analysis and are deferred:

- **Compiled patterns stored in variables**: `pat = r"^\d+$"; re.compile(pat)` — the pattern is an `identifier`, not a `string`. We would need to trace the variable binding, which is infeasible without a type system.
- **Pattern objects passed as arguments**: `def match_it(pattern): re.search(pattern, text)`.
- **`from re import compile`**: bare `compile(...)` calls are ambiguous (could be `re.compile` or something else). Too many false positives without import resolution.
- **f-strings containing regex**: `re.compile(f"^{prefix}\d+$")` — the pattern is dynamic at runtime. Mutating a fragment of it could produce nonsensical mutations.
- **String concatenation**: `re.compile("^" + pattern + "$")` — dynamic.
- **`re.Pattern` type hints**: Too fragile for call-site detection; ignore.

### String prefix handling

Python regex strings commonly use the `r` (raw) prefix. The tree-sitter `string` node includes the prefix and quotes. We need to extract just the pattern content:

```rust
/// Extract the inner pattern text from a Python string node,
/// stripping prefix (r, b, etc.) and quotes.
/// Returns (prefix, inner_pattern, quote_char).
fn extract_pattern_from_string(
    node: Node<'_>,
    source: &str,
) -> Option<(String, String, char)> {
    let text = node_text(source, node);

    // Find where the quote starts.
    let quote_start = text.find(['"', '\''])?;
    let prefix = &text[..quote_start];
    let quote_char = text.as_bytes()[quote_start] as char;

    // Reject triple-quoted strings — regex patterns are almost never triple-quoted,
    // and the mutation logic gets complex.
    if text[quote_start..].starts_with("\"\"\"") || text[quote_start..].starts_with("'''") {
        return None;
    }

    let inner = &text[quote_start + 1..text.len() - 1];
    Some((prefix.to_string(), inner.to_string(), quote_char))
}
```

---

## Parsing: How to Parse Regex Patterns in Rust

### Option A: `regex-syntax` crate (recommended)

The `regex-syntax` crate (from the `regex` ecosystem, maintained by BurntSushi) provides:

- A full **AST parser** (`regex_syntax::ast::parse::Parser`) that preserves source structure.
- An `Ast` enum with variants: `Empty`, `Flags`, `Literal`, `Dot`, `Assertion`, `ClassUnicode`, `ClassPerl`, `ClassBracketed`, `Repetition`, `Group`, `Alternation`, `Concat`.
- **`Display` impl on `Ast`** that reconstructs a regex string from the AST — critical for generating mutated patterns.
- `AssertionKind` with 12 variants including `StartLine`, `EndLine`, `StartText`, `EndText`, `WordBoundary`, `NotWordBoundary`.

**Tradeoffs**:

| Pro | Con |
|-----|-----|
| Battle-tested, correct parser | Targets Rust's `regex` flavor, not Python's `re` |
| Full AST with span info | No support for backreferences (`\1`), lookaheads/lookbehinds (`(?=...)`, `(?!...)`), `(?P<name>...)` |
| `Display` reconstructs pattern | Will reject valid Python patterns that use unsupported features |
| Zero new dependencies (already transitive via `regex`) | Need to handle parse failures gracefully |

**Python-specific syntax NOT supported by `regex-syntax`**:

| Python feature | Syntax | `regex-syntax` support |
|----------------|--------|----------------------|
| Named groups (Python style) | `(?P<name>...)` | No (`(?<name>...)` is supported, but not `(?P<...>)`) |
| Named backreferences | `(?P=name)` | No |
| Numeric backreferences | `\1`, `\2` | No (rejected as error) |
| Lookahead | `(?=...)`, `(?!...)` | No |
| Lookbehind | `(?<=...)`, `(?<!...)` | No |
| Conditional patterns | `(?(id)yes\|no)` | No |
| Atomic groups (3.11+) | `(?>...)` | No |
| Possessive quantifiers (3.11+) | `*+`, `++`, `?+` | No |
| Comments | `(?#...)` | No |
| Inline flags | `(?aiLmsux)` | Partial (different flag letters) |

**Mitigation strategy**: Parse the pattern with `regex-syntax`. If it fails (returns `Err`), fall back to the text-based approach (Option C below) for simple mutations, or skip the pattern entirely. In practice, many Python regex patterns are simple enough to be valid Rust `regex` syntax too — they use `\d`, `\w`, `^`, `$`, `[...]`, `+`, `*`, `?`, `{n,m}`, and alternation `|`, all of which `regex-syntax` handles fine.

### Option B: Custom Python regex parser

Write a purpose-built parser for Python's `re` module syntax.

| Pro | Con |
|-----|-----|
| Full Python `re` compatibility | Significant engineering effort (1000+ lines) |
| Handles lookaheads, backrefs, etc. | Bugs in the parser become bugs in mutations |
| No dependency constraints | Must track Python version changes |

**Verdict**: Not worth it for v1. The subset of patterns we can parse with `regex-syntax` covers the vast majority of real-world cases. The Python-only features (`(?P<name>...)`, lookaheads) are important but their internal structure rarely needs to be mutated — they mostly appear as-is and the interesting mutations (anchor removal, quantifier removal, class negation) apply to the parts `regex-syntax` *can* parse.

### Option C: Text-based pattern matching (minimal approach)

Use simple string/regex scanning for a handful of high-value mutations without any AST:

```rust
// Anchor removal: just strip ^ from start or $ from end
// \d ↔ \D: simple text replacement
// [^...] → [...]: remove ^ after [
```

| Pro | Con |
|-----|-----|
| Zero dependencies, trivial to implement | No understanding of nesting or escaping |
| Handles any Python regex flavor | Will produce false positives on escaped chars |
| Simple to maintain | Cannot do quantifier removal safely (needs to know what's quantified) |

**Verdict**: Use as a fallback when `regex-syntax` can't parse the pattern. Apply only the simplest, least ambiguous mutations (anchor removal, shorthand class negation) via text scanning.

### Recommended approach: Hybrid (Option A + C)

1. **Try `regex-syntax` first**. If it parses successfully, walk the AST and generate mutations from the structured representation. This gives us safe, correct mutations for the majority of patterns.

2. **On parse failure, fall back to text-based mutations**. Apply only the safe subset: anchor removal (`^`, `$`), shorthand class swap (`\d` ↔ `\D`, `\w` ↔ `\W`, `\s` ↔ `\S`). These are unambiguous even without understanding the full pattern structure.

3. **Track parse failure rate** in stats. If >20% of detected patterns fail to parse, that signals a need for a more Python-aware parser (Option B) in a future version.

---

## Operators: Concrete List Ranked by Value

### Tier 1 — Must-Have (v1)

These operators have the highest bug-detection value and are straightforward to implement.

| # | Operator | Original | Mutated | Rationale |
|---|----------|----------|---------|-----------|
| 1 | `regex_anchor_removal_start` | `^\d+` | `\d+` | Missing `^` is one of the most common regex bugs. Pattern matches in the middle of string instead of only at start. |
| 2 | `regex_anchor_removal_end` | `\d+$` | `\d+` | Missing `$` — matches prefix instead of full suffix. |
| 3 | `regex_charclass_negation` | `[abc]` | `[^abc]` | Inverted character class — accepts everything the original rejects. |
| 4 | `regex_charclass_negation_removal` | `[^abc]` | `[abc]` | Removes negation — accepts only what original rejects. |
| 5 | `regex_shorthand_negation` | `\d` | `\D` | Digit ↔ non-digit. Catches tests that don't verify the *kind* of character matched. |
| 6 | `regex_shorthand_negation` | `\w` | `\W` | Word ↔ non-word. Same rationale. |
| 7 | `regex_shorthand_negation` | `\s` | `\S` | Whitespace ↔ non-whitespace. |
| 8 | `regex_quantifier_removal` | `a+` | `a` | Tests that "one or more" is actually tested. Pattern matches only exactly one. |
| 9 | `regex_quantifier_removal` | `a*` | `a` | Tests that "zero or more" is actually tested. |
| 10 | `regex_quantifier_removal` | `a?` | `a` | Tests that optionality is tested. |
| 11 | `regex_quantifier_removal` | `a{2,5}` | `a` | Range quantifier removal. |
| 12 | `regex_lookaround_negation` | `(?=abc)` | `(?!abc)` | Positive ↔ negative lookahead. Only via text-based fallback since `regex-syntax` doesn't parse lookaheads. |
| 13 | `regex_lookaround_negation` | `(?<=abc)` | `(?<!abc)` | Positive ↔ negative lookbehind. Text-based only. |

**Expected value**: These 13 operators cover the most frequent regex bugs. Stryker's data shows anchor removal and quantifier removal alone catch ~40% of regex-related surviving mutants.

### Tier 2 — Nice-to-Have (v1)

| # | Operator | Original | Mutated | Rationale |
|---|----------|----------|---------|-----------|
| 14 | `regex_quantifier_change` | `a{2,5}` | `a{1,5}` / `a{3,5}` / `a{2,4}` / `a{2,6}` | Off-by-one in range quantifiers. |
| 15 | `regex_charclass_child_removal` | `[abc]` | `[bc]` / `[ac]` / `[ab]` | Remove individual elements from character classes. May generate many mutants. |
| 16 | `regex_group_to_noncapturing` | `(abc)` | `(?:abc)` | Tests that capture groups are actually used (e.g., `match.group(1)`). |
| 17 | `regex_dot_to_charclass` | `.` | `\w` | Dot matches "anything" — replace with something narrower to see if tests notice. |
| 18 | `regex_boundary_removal` | `\b` | (removed) | Word boundary removal. |
| 19 | `regex_flag_removal` | `re.compile(pat, re.IGNORECASE)` | `re.compile(pat)` | Remove regex flags. Separate from pattern mutation — operates on the flags argument. |
| 20 | `regex_alternation_removal` | `a\|b\|c` | `a\|b` / `a\|c` / `b\|c` | Remove individual alternatives. |

### Tier 3 — Exotic (v3 / never)

| # | Operator | Original | Mutated | Notes |
|---|----------|----------|---------|-------|
| 21 | `regex_quantifier_to_reluctant` | `a*` | `a*?` | Greedy ↔ reluctant. Often no observable difference. |
| 22 | `regex_charclass_range_mod` | `[a-z]` | `[b-z]` / `[a-y]` | Narrow ranges. Produces many equivalent mutants. |
| 23 | `regex_unicode_property_negation` | `\p{Alpha}` | `\P{Alpha}` | Rare in Python (requires `regex` module, not `re`). |
| 24 | `regex_backslash_nullification` | `\d` | `d` | Remove escape — turns class into literal. Mostly caught by Tier 1. |
| 25 | `regex_charclass_to_any` | `[abc]` | `[\w\W]` | Character class to match-everything. Mostly caught by negation. |
| 26 | `regex_capturing_to_noncapturing` | `(...)` | `(?:...)` | Only useful if `.group()` calls are tested. |

---

## Integration: Fitting into the Existing Architecture

### New module: `src/regex_mutation.rs`

Regex mutation is complex enough to warrant its own module rather than extending `tree_sitter_mutation.rs` further. The module boundary:

- **`tree_sitter_mutation.rs`**: Detects `re.<func>(...)` call nodes, extracts the string argument, calls into `regex_mutation.rs`.
- **`regex_mutation.rs`**: Receives the pattern string, parses it, generates `Mutation` structs with byte offsets relative to the string node.

```rust
// src/regex_mutation.rs

use crate::mutation::Mutation;

/// Generate regex mutations for a pattern string found in source code.
///
/// `pattern_text` is the raw Python string node text, e.g. `r"^\d+$"` or `"\\d+"`.
/// `node_start` is the byte offset of the string node relative to fn_start.
/// Returns mutations with byte offsets relative to fn_start.
pub fn collect_regex_mutations(
    pattern_text: &str,
    node_start: usize,
) -> Vec<Mutation> {
    let Some((prefix, inner, quote)) = extract_pattern_parts(pattern_text) else {
        return vec![];
    };

    // Byte offset where the inner pattern starts within the full source.
    // prefix length + 1 for the opening quote.
    let inner_offset = node_start + prefix.len() + 1;

    let mut mutations = Vec::new();

    // Try AST-based mutations first.
    if let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(&inner) {
        collect_ast_mutations(&ast, &inner, inner_offset, &mut mutations);
    }

    // Always attempt text-based mutations for constructs regex-syntax can't parse.
    // Deduplicate against AST-produced mutations.
    collect_text_based_mutations(&inner, inner_offset, quote, &mut mutations);

    // Wrap each mutation's replacement to reconstruct the full string literal.
    // Mutations target byte ranges within the inner pattern, but the Mutation
    // struct's start/end are relative to fn_start, so they already account
    // for the prefix + quote offset. The replacement text is just the
    // mutated substring within the pattern — apply_mutation splices it in.
    mutations
}
```

### Hook point in `tree_sitter_mutation.rs`

Add regex detection to the existing `"call"` match arm in `collect_node_mutations`:

```rust
// In collect_node_mutations, under the "call" arm:
"call" => {
    add_arg_removal_mutations(node, source, fn_start, mutations);
    add_method_mutations(node, source, fn_start, mutations);
    add_dict_kwarg_mutations(node, source, fn_start, mutations);
    // NEW: regex mutations
    add_regex_mutations(node, source, fn_start, mutations);
}
```

```rust
fn add_regex_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(string_node) = detect_regex_call(node, source) else {
        return;
    };
    let pattern_text = node_text(source, string_node);
    let node_start = string_node.start_byte() - fn_start;
    let regex_muts = crate::regex_mutation::collect_regex_mutations(pattern_text, node_start);
    mutations.extend(regex_muts);
}
```

### Operator naming in `Mutation.operator`

All regex mutations use operator names prefixed with `regex_`:

```
regex_anchor_removal
regex_charclass_negation
regex_shorthand_negation
regex_quantifier_removal
regex_quantifier_change
regex_lookaround_negation
regex_charclass_child_removal
regex_group_to_noncapturing
regex_boundary_removal
regex_flag_removal
regex_alternation_removal
```

This keeps them grouped in reports and makes it easy to filter/disable them.

### Mutation generation from AST

The core walk function visits each AST node and emits mutations:

```rust
use regex_syntax::ast::{self, Ast, AssertionKind, ClassPerlKind, RepetitionKind};

fn collect_ast_mutations(
    ast_node: &Ast,
    pattern: &str,
    inner_offset: usize,
    mutations: &mut Vec<Mutation>,
) {
    match ast_node {
        Ast::Assertion(assertion) => {
            // Tier 1: Anchor removal
            let span = &assertion.span;
            let start = inner_offset + span.start.offset;
            let end = inner_offset + span.end.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            match assertion.kind {
                AssertionKind::StartLine | AssertionKind::StartText => {
                    mutations.push(Mutation {
                        start,
                        end,
                        original: original.to_string(),
                        replacement: String::new(), // remove anchor
                        operator: "regex_anchor_removal",
                    });
                }
                AssertionKind::EndLine | AssertionKind::EndText => {
                    mutations.push(Mutation {
                        start,
                        end,
                        original: original.to_string(),
                        replacement: String::new(),
                        operator: "regex_anchor_removal",
                    });
                }
                AssertionKind::WordBoundary => {
                    mutations.push(Mutation {
                        start,
                        end,
                        original: original.to_string(),
                        replacement: String::new(),
                        operator: "regex_boundary_removal",
                    });
                }
                AssertionKind::NotWordBoundary => {
                    mutations.push(Mutation {
                        start,
                        end,
                        original: original.to_string(),
                        replacement: String::new(),
                        operator: "regex_boundary_removal",
                    });
                }
                _ => {}
            }
        }

        Ast::ClassPerl(class) => {
            // Tier 1: Shorthand class negation (\d ↔ \D, \w ↔ \W, \s ↔ \S)
            let span = &class.span;
            let start = inner_offset + span.start.offset;
            let end = inner_offset + span.end.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            let replacement = match (class.kind, class.negated) {
                (ClassPerlKind::Digit, false) => r"\D",
                (ClassPerlKind::Digit, true) => r"\d",
                (ClassPerlKind::Space, false) => r"\S",
                (ClassPerlKind::Space, true) => r"\s",
                (ClassPerlKind::Word, false) => r"\W",
                (ClassPerlKind::Word, true) => r"\w",
            };
            mutations.push(Mutation {
                start,
                end,
                original: original.to_string(),
                replacement: replacement.to_string(),
                operator: "regex_shorthand_negation",
            });
        }

        Ast::ClassBracketed(class) => {
            // Tier 1: Character class negation [abc] ↔ [^abc]
            let span = &class.span;
            let start = inner_offset + span.start.offset;
            let end = inner_offset + span.end.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            if class.negated {
                // [^abc] → [abc]: remove the ^ after [
                let mut replacement = original.to_string();
                if replacement.starts_with("[^") {
                    replacement = format!("[{}", &replacement[2..]);
                }
                mutations.push(Mutation {
                    start,
                    end,
                    original: original.to_string(),
                    replacement,
                    operator: "regex_charclass_negation",
                });
            } else {
                // [abc] → [^abc]: insert ^ after [
                let replacement = format!("[^{}", &original[1..]);
                mutations.push(Mutation {
                    start,
                    end,
                    original: original.to_string(),
                    replacement,
                    operator: "regex_charclass_negation",
                });
            }

            // Recurse into the class body (for nested classes, ranges, etc.)
            // The ClassBracketed body is handled by regex-syntax's AST already.
        }

        Ast::Repetition(rep) => {
            // Tier 1: Quantifier removal — replace `X+` with `X`, `X*` with `X`, etc.
            let span = &rep.span;
            let start = inner_offset + span.start.offset;
            let end = inner_offset + span.end.offset;
            let original = &pattern[span.start.offset..span.end.offset];

            // The replacement is just the sub-expression without the quantifier.
            let sub_span = &rep.ast.span();
            let sub_text = &pattern[sub_span.start.offset..sub_span.end.offset];

            mutations.push(Mutation {
                start,
                end,
                original: original.to_string(),
                replacement: sub_text.to_string(),
                operator: "regex_quantifier_removal",
            });

            // Tier 2: Quantifier range changes (stretch goal)
            if let RepetitionKind::Range(range) = &rep.op.kind {
                // Generate off-by-one mutations for {n,m} ranges.
                add_quantifier_range_mutations(
                    range, rep, pattern, inner_offset, mutations,
                );
            }

            // Recurse into the quantified sub-expression.
            collect_ast_mutations(&rep.ast, pattern, inner_offset, mutations);
            return; // Don't recurse again at the bottom.
        }

        Ast::Group(group) => {
            // Recurse into group body.
            collect_ast_mutations(&group.ast, pattern, inner_offset, mutations);
            return;
        }

        Ast::Alternation(alt) => {
            // Tier 2: Alternation branch removal (if >2 branches).
            if alt.asts.len() > 1 {
                add_alternation_removal_mutations(
                    alt, pattern, inner_offset, mutations,
                );
            }
            // Recurse into each alternative.
            for ast in &alt.asts {
                collect_ast_mutations(ast, pattern, inner_offset, mutations);
            }
            return;
        }

        Ast::Concat(concat) => {
            for ast in &concat.asts {
                collect_ast_mutations(ast, pattern, inner_offset, mutations);
            }
            return;
        }

        _ => {}
    }

    // Default recursion for non-compound nodes is not needed — compound
    // nodes (Repetition, Group, Alternation, Concat) handle their own recursion.
}
```

### Text-based fallback mutations

For patterns that `regex-syntax` rejects (those with lookaheads, backreferences, etc.):

```rust
fn collect_text_based_mutations(
    inner: &str,
    inner_offset: usize,
    _quote: char,
    mutations: &mut Vec<Mutation>,
) {
    // Anchor removal: ^ at very start of pattern.
    if inner.starts_with('^') {
        maybe_push_unique(mutations, Mutation {
            start: inner_offset,
            end: inner_offset + 1,
            original: "^".to_string(),
            replacement: String::new(),
            operator: "regex_anchor_removal",
        });
    }

    // Anchor removal: $ at very end of pattern.
    if inner.ends_with('$') && !inner.ends_with("\\$") {
        let offset = inner_offset + inner.len() - 1;
        maybe_push_unique(mutations, Mutation {
            start: offset,
            end: offset + 1,
            original: "$".to_string(),
            replacement: String::new(),
            operator: "regex_anchor_removal",
        });
    }

    // Shorthand class negation: scan for \d, \D, \w, \W, \s, \S.
    let class_swaps = &[
        (r"\d", r"\D"), (r"\D", r"\d"),
        (r"\w", r"\W"), (r"\W", r"\w"),
        (r"\s", r"\S"), (r"\S", r"\s"),
    ];
    for (from, to) in class_swaps {
        for (byte_pos, _) in inner.match_indices(from) {
            // Verify this isn't inside a character class or escaped.
            // Simple heuristic: check that the preceding char isn't `\`.
            if byte_pos > 0 && inner.as_bytes()[byte_pos - 1] == b'\\' {
                continue; // \\d is an escaped backslash + literal d, not \d
            }
            maybe_push_unique(mutations, Mutation {
                start: inner_offset + byte_pos,
                end: inner_offset + byte_pos + from.len(),
                original: from.to_string(),
                replacement: to.to_string(),
                operator: "regex_shorthand_negation",
            });
        }
    }

    // Lookaround negation (text-based, since regex-syntax can't parse these).
    let lookaround_swaps = &[
        ("(?=", "(?!"),   // positive → negative lookahead
        ("(?!", "(?="),   // negative → positive lookahead
        ("(?<=", "(?<!"), // positive → negative lookbehind
        ("(?<!", "(?<="), // negative → positive lookbehind
    ];
    for (from, to) in lookaround_swaps {
        for (byte_pos, _) in inner.match_indices(from) {
            mutations.push(Mutation {
                start: inner_offset + byte_pos,
                end: inner_offset + byte_pos + from.len(),
                original: from.to_string(),
                replacement: to.to_string(),
                operator: "regex_lookaround_negation",
            });
        }
    }
}

/// Push a mutation only if no existing mutation covers the same span and operator.
fn maybe_push_unique(mutations: &mut Vec<Mutation>, mutation: Mutation) {
    let dominated = mutations.iter().any(|m| {
        m.start == mutation.start
            && m.end == mutation.end
            && m.operator == mutation.operator
    });
    if !dominated {
        mutations.push(mutation);
    }
}
```

---

## Edge Cases

### Raw strings vs regular strings

Python raw strings (`r"..."`) and regular strings (`"..."`) contain different escape sequences. For mutation purposes:

- In `r"\d+"`, the `\d` is literally the two characters `\` and `d`. The regex engine sees `\d`.
- In `"\d+"`, Python's string parser interprets `\d` — but `\d` isn't a recognized Python escape, so it passes through as `\d`. (This is a `DeprecationWarning` in modern Python.)
- In `"\\d+"`, the `\\` becomes `\`, so the regex engine sees `\d`.

**Strategy**: We operate on the tree-sitter string node text, which is the source-level representation. When we detect `\d` in a raw string `r"\d+"`, we know it represents the regex class `\d`. When we detect `\\d` in a regular string `"\\d+"`, we also know it represents `\d`. The mutations replace source text with source text of the same string kind.

For AST-based mutations via `regex-syntax`, we need to feed it the *interpreted* pattern (what the regex engine sees). For raw strings, that's the inner text as-is. For non-raw strings, we must interpret Python escape sequences first. **v1 simplification**: only mutate patterns in raw strings (`r"..."`) and byte-prefixed raw strings (`rb"..."`). Regular strings with regex patterns are a code smell anyway — linters flag them.

### f-strings with regex

Skip entirely. Tree-sitter parses f-strings as `formatted_string` nodes (not `string`), and the first-positional-arg check already requires `kind() == "string"`.

### Compiled patterns stored in variables

```python
EMAIL_RE = re.compile(r"^[^@]+@[^@]+\.[^@]+$")
```

This is handled: `re.compile(...)` is detected at the call site, and the string literal is right there as the argument. The pattern is mutated in place within the `re.compile(...)` call.

What is NOT handled:

```python
PATTERN = r"^[^@]+@[^@]+\.[^@]+$"
compiled = re.compile(PATTERN)
```

Here the string literal is assigned to `PATTERN` and the `re.compile` call receives an identifier, not a string. We skip this case in v1.

### Flags as arguments

```python
re.compile(r"hello", re.IGNORECASE | re.MULTILINE)
```

**v1**: We do not mutate the flags argument. The pattern string (first positional arg) is the target.

**v2 (flag removal)**: Detect flag arguments and generate mutations that remove individual flags. This operates on the call's argument list, not the pattern string, so it's a separate operator (`regex_flag_removal`) that lives alongside `add_arg_removal_mutations`.

### `re.VERBOSE` / `re.X` patterns

Verbose patterns use `#` for comments and ignore whitespace:

```python
re.compile(r"""
    ^           # start of string
    [a-zA-Z]+   # one or more letters
    \d{2,4}     # 2-4 digits
    $           # end of string
""", re.VERBOSE)
```

`regex-syntax` has a `parse_with_comments` method, but the comment syntax differs from Python's verbose mode. **v1**: Skip triple-quoted strings (already filtered out by `extract_pattern_parts`). This avoids most verbose patterns. If a verbose pattern uses a single-line raw string, we try to parse it normally — `regex-syntax` will either handle it or fail, and the fallback text mutations are still safe.

### Multiline patterns

Patterns that span multiple lines via string concatenation or triple-quoting:

```python
re.compile(
    r"^(\d{4})-"
    r"(\d{2})-"
    r"(\d{2})$"
)
```

This is actually implicit string concatenation in Python. Tree-sitter represents this as a `concatenated_string` node, not a single `string` node. **v1**: `detect_regex_call` requires the argument to be a `string` node. Concatenated strings are skipped. They could be supported in v2 by joining the fragments and adjusting offsets, but the bookkeeping is error-prone.

---

## Testing Strategy

### Unit tests in `regex_mutation.rs`

Each operator gets a focused test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anchor_removal_start() {
        let mutations = collect_regex_mutations(r#"r"^\d+$""#, 0);
        let anchors: Vec<_> = mutations.iter()
            .filter(|m| m.operator == "regex_anchor_removal")
            .collect();
        assert_eq!(anchors.len(), 2); // ^ and $
        assert_eq!(anchors[0].original, "^");
        assert_eq!(anchors[0].replacement, "");
    }

    #[test]
    fn test_shorthand_negation() {
        let mutations = collect_regex_mutations(r#"r"\d\w\s""#, 0);
        let negs: Vec<_> = mutations.iter()
            .filter(|m| m.operator == "regex_shorthand_negation")
            .collect();
        assert_eq!(negs.len(), 3);
        // \d → \D, \w → \W, \s → \S
    }

    #[test]
    fn test_quantifier_removal() {
        let mutations = collect_regex_mutations(r#"r"\d+\w*\s?""#, 0);
        let quants: Vec<_> = mutations.iter()
            .filter(|m| m.operator == "regex_quantifier_removal")
            .collect();
        assert_eq!(quants.len(), 3);
    }

    #[test]
    fn test_charclass_negation() {
        let mutations = collect_regex_mutations(r#"r"[abc]""#, 0);
        let negs: Vec<_> = mutations.iter()
            .filter(|m| m.operator == "regex_charclass_negation")
            .collect();
        assert_eq!(negs.len(), 1);
        assert!(negs[0].replacement.contains("[^abc]"));
    }

    #[test]
    fn test_lookaround_text_fallback() {
        // regex-syntax can't parse lookaheads, so this exercises the text fallback.
        let mutations = collect_regex_mutations(r#"r"(?=foo)bar""#, 0);
        let looks: Vec<_> = mutations.iter()
            .filter(|m| m.operator == "regex_lookaround_negation")
            .collect();
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].replacement, "(?!");
    }

    #[test]
    fn test_non_regex_string_not_mutated() {
        // A plain string not inside re.compile should not get regex mutations.
        // This is enforced at the tree_sitter_mutation.rs level, not here.
    }

    #[test]
    fn test_fstring_skipped() {
        // f-strings should not be detected as regex patterns.
    }

    #[test]
    fn test_non_raw_string_skipped_v1() {
        // Non-raw strings like "\d+" should be skipped in v1
        // to avoid escape interpretation bugs.
    }
}
```

### Integration tests via `tree_sitter_mutation.rs`

Full-pipeline tests that verify regex mutations appear in `FunctionMutations`:

```rust
#[test]
fn test_regex_mutations_in_function() {
    let source = r#"
import re

def validate_email(email):
    return re.match(r"^[^@]+@[^@]+\.[^@]+$", email) is not None
"#;
    let fms = collect_file_mutations(source);
    assert_eq!(fms.len(), 1);
    let regex_muts: Vec<_> = fms[0].mutations.iter()
        .filter(|m| m.operator.starts_with("regex_"))
        .collect();
    // Should find: 2 anchor removals, 2 charclass negations, shorthand negations
    assert!(regex_muts.len() >= 4);
}
```

### Fixture files

Add test fixtures in `tests/fixtures/`:

```
tests/fixtures/regex_project/
├── pyproject.toml
├── src/
│   └── validators.py    # Functions using re.compile, re.match, etc.
└── tests/
    └── test_validators.py
```

`validators.py` should contain a mix of:
- Simple patterns: `r"^\d+$"`, `r"[a-z]+"`, `r"\w+@\w+\.\w+"`
- Patterns with lookaheads: `r"(?=.*[A-Z])(?=.*\d).{8,}"`
- Compiled patterns: `PATTERN = re.compile(r"...")`
- Patterns with flags: `re.compile(r"hello", re.IGNORECASE)`
- Edge cases: f-strings, concatenated strings, non-raw strings

### Proptest strategies

Use proptest to verify that mutations produce syntactically valid Python:

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn regex_mutations_produce_valid_python(
        pattern in r"[\^$\\.+*?\[\](){}|\\d\\w\\s]{1,30}"
    ) {
        let source = format!(
            "import re\ndef f():\n    re.compile(r\"{}\")\n",
            pattern
        );
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                if m.operator.starts_with("regex_") {
                    let mutated = apply_mutation(&fm.source, m);
                    // The mutated source should still be valid Python
                    // (the regex might be invalid, but the Python syntax is fine).
                    assert!(parses_as_python(&format!(
                        "import re\n{}", mutated
                    )));
                }
            }
        }
    }
}
```

---

## Scope and Phasing

### v1 (initial implementation)

**Goal**: Ship all Tier 1 + Tier 2 regex mutations. On by default, no disable flag.

**In scope**:
- Detection of `re.<func>(pattern)` and `regex.<func>(pattern)` call sites via tree-sitter
- Pattern extraction from raw strings only (`r"..."`, `r'...'`)
- `regex-syntax` AST-based mutations: all Tier 1 + Tier 2 operators (anchor removal, charclass negation, shorthand negation, quantifier removal, quantifier range changes, charclass child removal, group-to-noncapturing, dot-to-charclass, boundary removal, alternation removal)
- Text-based fallback mutations: anchor removal, shorthand negation, lookaround negation
- Deduplication between AST and text-based results
- New module `src/regex_mutation.rs`
- Unit tests for each operator
- Integration test with fixture project
- Operator names prefixed with `regex_` in reports

**Out of scope**:
- Non-raw string patterns (escape handling)
- Concatenated string patterns
- Variable-stored patterns
- `from re import compile` bare calls
- Flag removal mutations (operates on call args, not pattern — separate operator)
- `re.Pattern` type hints

**Dependency change**: Add `regex-syntax = "0.8"` to `Cargo.toml` `[dependencies]`.

### Future

- Non-raw string pattern support (interpret Python escape sequences)
- Flag removal mutations (`re.IGNORECASE` → removed)
- Concatenated string patterns
- Variable-tracked patterns (lightweight dataflow: `X = r"..."; re.compile(X)`)
- `from re import compile` with import resolution
- Custom Python regex parser if `regex-syntax` failure rate is high (see #19)
- Verbose pattern support

---

## Dependency Evaluation: `regex-syntax`

| Criterion | Assessment |
|-----------|------------|
| **Maintenance** | Actively maintained by BurntSushi (Andrew Galloway), part of the `regex` crate ecosystem. 400M+ downloads on crates.io. |
| **Size** | ~25K lines of Rust. Compile-time impact: adds ~2-3 seconds to clean build. |
| **Transitive deps** | Zero — `regex-syntax` has no dependencies. |
| **API stability** | Follows semver. Major versions are infrequent. Current: 0.8.x. |
| **License** | MIT / Apache-2.0 dual license. Compatible with irradiate's MIT license. |
| **Alternative** | Could vendor a minimal regex parser, but the engineering cost far outweighs the dependency cost. |

**Recommendation**: Add `regex-syntax = "0.8"` as a dependency. The crate is lightweight, zero-dep, and provides exactly the structured AST we need.

---

## Decisions

- **On by default**: No disable flag. Regex patterns are rare per file, so noise is low.
- **Ignore `re.Pattern` type hints**: Too fragile, not worth pursuing.
- **Count in main mutation total**: Regex mutations are mutations like any other. Per-category reporting is tracked in #20.
- **Performance**: Not a concern. Regex patterns are rare (0-5 per file in regex-heavy code, 0 in most). No caching needed.
- **Support both `re` and `regex` modules** from the start.
