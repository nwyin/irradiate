//! Regex pattern mutation operators.
//!
//! Generates mutations for regex patterns found in `re.<func>(...)` and
//! `regex.<func>(...)` call sites. Uses the `regex-syntax` crate for
//! AST-based mutations, with text-based fallback for Python-specific
//! constructs (lookaheads, backreferences) that `regex-syntax` can't parse.

use crate::mutation::Mutation;
use regex_syntax::ast::{
    Ast, AssertionKind, ClassBracketed, ClassPerlKind, ClassSet, ClassSetItem, GroupKind,
    Repetition, RepetitionKind, RepetitionRange,
};

/// Generate regex mutations for a pattern string found in source code.
///
/// `pattern_text` is the raw Python string node text, e.g. `r"^\d+$"` or `r'\w+'`.
/// `node_start` is the byte offset of the string node relative to fn_start.
///
/// Returns mutations with byte offsets relative to fn_start.
pub fn collect_regex_mutations(pattern_text: &str, node_start: usize) -> Vec<Mutation> {
    let Some((prefix_len, inner, quote)) = extract_pattern_parts(pattern_text) else {
        return vec![];
    };

    // Byte offset where the inner pattern starts, relative to fn_start.
    let inner_offset = node_start + prefix_len + 1; // +1 for the opening quote

    let mut mutations = Vec::new();

    // Try AST-based mutations first.
    if let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(inner) {
        collect_ast_mutations(&ast, inner, inner_offset, &mut mutations);
    }

    // Always attempt text-based mutations for constructs regex-syntax can't parse.
    // Deduplicate against AST-produced mutations.
    collect_text_based_mutations(inner, inner_offset, quote, &mut mutations);

    mutations
}

/// Extract the inner pattern from a Python string node.
///
/// Returns `(prefix_len, inner_pattern, quote_char)` or `None` if the string
/// is not suitable for regex mutation (non-raw, triple-quoted, byte string).
///
/// v1: Only raw strings (`r"..."`, `r'...'`) are supported. Non-raw strings
/// require Python escape interpretation which is error-prone.
fn extract_pattern_parts(text: &str) -> Option<(usize, &str, char)> {
    // Find where the quote starts.
    let quote_start = text.find(['"', '\''])?;
    let prefix = &text[..quote_start];

    // v1: only raw strings. Must have 'r' or 'R' in prefix, and no 'b'/'B' (byte strings).
    let lower_prefix = prefix.to_ascii_lowercase();
    if !lower_prefix.contains('r') {
        return None;
    }
    if lower_prefix.contains('b') {
        return None;
    }

    let quote_char = text.as_bytes()[quote_start] as char;

    // Reject triple-quoted strings.
    let after_quote = &text[quote_start..];
    if after_quote.starts_with("\"\"\"") || after_quote.starts_with("'''") {
        return None;
    }

    // Inner pattern is between the opening and closing quotes.
    let inner = &text[quote_start + 1..text.len() - 1];
    Some((prefix.len(), inner, quote_char))
}

// ────────────────────── AST-Based Mutations ──────────────────────

/// Recursively walk a regex-syntax AST and generate mutations.
fn collect_ast_mutations(
    ast_node: &Ast,
    pattern: &str,
    inner_offset: usize,
    mutations: &mut Vec<Mutation>,
) {
    match ast_node {
        Ast::Assertion(assertion) => {
            let span = &assertion.span;
            let start = inner_offset + span.start.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            match assertion.kind {
                // Tier 1: Anchor removal (^, $, \A, \z)
                AssertionKind::StartLine
                | AssertionKind::EndLine
                | AssertionKind::StartText
                | AssertionKind::EndText => {
                    mutations.push(make_mutation(original, "", "regex_anchor_removal", start));
                }
                // Tier 1: Boundary removal (\b, \B)
                AssertionKind::WordBoundary
                | AssertionKind::NotWordBoundary
                | AssertionKind::WordBoundaryStart
                | AssertionKind::WordBoundaryEnd
                | AssertionKind::WordBoundaryStartAngle
                | AssertionKind::WordBoundaryEndAngle => {
                    mutations.push(make_mutation(original, "", "regex_boundary_removal", start));
                }
                _ => {}
            }
        }

        Ast::ClassPerl(class) => {
            // Tier 1: Shorthand class negation (\d ↔ \D, \w ↔ \W, \s ↔ \S)
            let span = &class.span;
            let start = inner_offset + span.start.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            let replacement = match (&class.kind, class.negated) {
                (ClassPerlKind::Digit, false) => r"\D",
                (ClassPerlKind::Digit, true) => r"\d",
                (ClassPerlKind::Space, false) => r"\S",
                (ClassPerlKind::Space, true) => r"\s",
                (ClassPerlKind::Word, false) => r"\W",
                (ClassPerlKind::Word, true) => r"\w",
            };
            mutations.push(make_mutation(original, replacement, "regex_shorthand_negation", start));
        }

        Ast::ClassBracketed(class) => {
            // Tier 1: Character class negation [abc] ↔ [^abc]
            let span = &class.span;
            let start = inner_offset + span.start.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            if class.negated {
                // [^abc] → [abc]: remove the ^ after [
                if let Some(rest) = original.strip_prefix("[^") {
                    let replacement = format!("[{rest}");
                    mutations.push(make_mutation(original, &replacement, "regex_charclass_negation", start));
                }
            } else {
                // [abc] → [^abc]: insert ^ after [
                let replacement = format!("[^{}", &original[1..]);
                mutations.push(make_mutation(original, &replacement, "regex_charclass_negation", start));
            }
            // Tier 2: Character class child removal
            add_charclass_child_removal_mutations(class, pattern, inner_offset, mutations);
        }

        Ast::Repetition(rep) => {
            // Tier 1: Quantifier removal — replace `X+` with `X`
            let span = &rep.span;
            let start = inner_offset + span.start.offset;
            let original = &pattern[span.start.offset..span.end.offset];

            // The replacement is just the sub-expression without the quantifier.
            let sub_span = rep.ast.span();
            let sub_text = &pattern[sub_span.start.offset..sub_span.end.offset];
            mutations.push(make_mutation(original, sub_text, "regex_quantifier_removal", start));

            // Tier 2: Quantifier range changes (off-by-one on {n,m})
            add_quantifier_range_mutations(rep, pattern, inner_offset, mutations);

            // Recurse into the quantified sub-expression.
            collect_ast_mutations(&rep.ast, pattern, inner_offset, mutations);
        }

        Ast::Dot(span) => {
            // Tier 2: Dot to charclass (. → \w)
            let start = inner_offset + span.start.offset;
            let original = &pattern[span.start.offset..span.end.offset];
            mutations.push(make_mutation(original, r"\w", "regex_dot_to_charclass", start));
        }

        Ast::Group(group) => {
            // Tier 2: Capturing group → non-capturing
            if let GroupKind::CaptureIndex(_) = group.kind {
                let span = &group.span;
                let start = inner_offset + span.start.offset;
                let original = &pattern[span.start.offset..span.end.offset];
                // (inner) → (?:inner): replace leading ( with (?:
                let replacement = format!("(?:{}", &original[1..]);
                mutations.push(make_mutation(original, &replacement, "regex_group_to_noncapturing", start));
            }
            // Recurse into group body.
            collect_ast_mutations(&group.ast, pattern, inner_offset, mutations);
        }

        Ast::Alternation(alt) => {
            // Tier 2: Alternation branch removal
            if alt.asts.len() >= 2 {
                add_alternation_removal_mutations(alt, pattern, inner_offset, mutations);
            }
            // Recurse into each alternative.
            for ast in &alt.asts {
                collect_ast_mutations(ast, pattern, inner_offset, mutations);
            }
        }

        Ast::Concat(concat) => {
            for ast in &concat.asts {
                collect_ast_mutations(ast, pattern, inner_offset, mutations);
            }
        }

        _ => {}
    }
}

// ────────────────────── Tier 2 Helpers ──────────────────────

/// Generate off-by-one mutations for `{n,m}` range quantifiers.
fn add_quantifier_range_mutations(
    rep: &Repetition,
    pattern: &str,
    inner_offset: usize,
    mutations: &mut Vec<Mutation>,
) {
    let RepetitionKind::Range(ref range) = rep.op.kind else {
        return;
    };
    let op_start = inner_offset + rep.op.span.start.offset;
    let original = &pattern[rep.op.span.start.offset..rep.op.span.end.offset];
    let lazy = if rep.greedy { "" } else { "?" };

    let variants: Vec<String> = match *range {
        RepetitionRange::Exactly(n) => {
            let mut v = vec![];
            if n > 0 {
                v.push(format!("{{{}}}{}", n - 1, lazy));
            }
            v.push(format!("{{{}}}{}", n + 1, lazy));
            v
        }
        RepetitionRange::AtLeast(n) => {
            let mut v = vec![];
            if n > 0 {
                v.push(format!("{{{},}}{}", n - 1, lazy));
            }
            v.push(format!("{{{},}}{}", n + 1, lazy));
            v
        }
        RepetitionRange::Bounded(m, n) => {
            let mut v = vec![];
            if m > 0 {
                v.push(format!("{{{},{}}}{}", m - 1, n, lazy));
            }
            if m < n {
                v.push(format!("{{{},{}}}{}", m + 1, n, lazy));
            }
            if n > 0 && n > m {
                v.push(format!("{{{},{}}}{}", m, n - 1, lazy));
            }
            v.push(format!("{{{},{}}}{}", m, n + 1, lazy));
            v
        }
    };

    for replacement in variants {
        mutations.push(make_mutation(original, &replacement, "regex_quantifier_change", op_start));
    }
}

/// Remove individual items from a bracketed character class.
fn add_charclass_child_removal_mutations(
    class: &ClassBracketed,
    pattern: &str,
    inner_offset: usize,
    mutations: &mut Vec<Mutation>,
) {
    // Only handle union-style class sets (not binary operations like &&, --, ~~).
    let ClassSet::Item(ClassSetItem::Union(ref union)) = class.kind else {
        return;
    };

    // Need at least 2 items to remove one and still have a valid class.
    if union.items.len() < 2 {
        return;
    }

    let class_start = inner_offset + class.span.start.offset;
    let original = &pattern[class.span.start.offset..class.span.end.offset];
    let negation = if class.negated { "^" } else { "" };

    for skip_idx in 0..union.items.len() {
        let mut parts = Vec::new();
        for (j, item) in union.items.iter().enumerate() {
            if j == skip_idx {
                continue;
            }
            let item_span = item.span();
            parts.push(&pattern[item_span.start.offset..item_span.end.offset]);
        }
        let replacement = format!("[{}{}]", negation, parts.join(""));
        mutations.push(make_mutation(original, &replacement, "regex_charclass_child_removal", class_start));
    }
}

/// Remove individual branches from an alternation.
fn add_alternation_removal_mutations(
    alt: &regex_syntax::ast::Alternation,
    pattern: &str,
    inner_offset: usize,
    mutations: &mut Vec<Mutation>,
) {
    let alt_start = inner_offset + alt.span.start.offset;
    let original = &pattern[alt.span.start.offset..alt.span.end.offset];

    for skip_idx in 0..alt.asts.len() {
        let mut parts = Vec::new();
        for (j, ast) in alt.asts.iter().enumerate() {
            if j == skip_idx {
                continue;
            }
            let s = ast.span();
            parts.push(&pattern[s.start.offset..s.end.offset]);
        }
        let replacement = parts.join("|");
        mutations.push(make_mutation(original, &replacement, "regex_alternation_removal", alt_start));
    }
}

// ────────────────────── Text-Based Fallback ──────────────────────

/// Apply simple text-based mutations for constructs regex-syntax can't parse.
///
/// Always runs after AST-based mutations. Uses `maybe_push_unique` to avoid
/// duplicating mutations already found by the AST walk.
fn collect_text_based_mutations(
    inner: &str,
    inner_offset: usize,
    _quote: char,
    mutations: &mut Vec<Mutation>,
) {
    // Anchor removal: ^ at very start of pattern.
    if inner.starts_with('^') {
        maybe_push_unique(mutations, make_mutation("^", "", "regex_anchor_removal", inner_offset));
    }

    // Anchor removal: $ at very end of pattern (guard against \$).
    if inner.ends_with('$') && !inner.ends_with("\\$") {
        let offset = inner_offset + inner.len() - 1;
        maybe_push_unique(mutations, make_mutation("$", "", "regex_anchor_removal", offset));
    }

    // Shorthand class negation: scan for \d, \D, \w, \W, \s, \S.
    let class_swaps: &[(&str, &str)] = &[
        (r"\d", r"\D"),
        (r"\D", r"\d"),
        (r"\w", r"\W"),
        (r"\W", r"\w"),
        (r"\s", r"\S"),
        (r"\S", r"\s"),
    ];
    for (from, to) in class_swaps {
        for (byte_pos, _) in inner.match_indices(from) {
            // Guard: if preceded by another backslash, this is \\d (escaped backslash + literal d).
            if byte_pos > 0 && inner.as_bytes()[byte_pos - 1] == b'\\' {
                continue;
            }
            maybe_push_unique(
                mutations,
                make_mutation(from, to, "regex_shorthand_negation", inner_offset + byte_pos),
            );
        }
    }

    // Lookaround negation (text-based only, since regex-syntax can't parse these).
    let lookaround_swaps: &[(&str, &str)] = &[
        ("(?=", "(?!"),
        ("(?!", "(?="),
        ("(?<=", "(?<!"),
        ("(?<!", "(?<="),
    ];
    for (from, to) in lookaround_swaps {
        for (byte_pos, _) in inner.match_indices(from) {
            mutations.push(make_mutation(from, to, "regex_lookaround_negation", inner_offset + byte_pos));
        }
    }
}

// ────────────────────── Helpers ──────────────────────

/// Push a mutation only if no existing mutation has the same (start, end, operator).
fn maybe_push_unique(mutations: &mut Vec<Mutation>, mutation: Mutation) {
    let dominated = mutations.iter().any(|m| {
        m.start == mutation.start && m.end == mutation.end && m.operator == mutation.operator
    });
    if !dominated {
        mutations.push(mutation);
    }
}

/// Convenience constructor for a Mutation.
fn make_mutation(original: &str, replacement: &str, operator: &'static str, start: usize) -> Mutation {
    Mutation {
        start,
        end: start + original.len(),
        original: original.to_string(),
        replacement: replacement.to_string(),
        operator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_pattern_parts ──

    #[test]
    fn extract_raw_double_quote() {
        let (prefix_len, inner, quote) = extract_pattern_parts(r#"r"^\d+$""#).unwrap();
        assert_eq!(prefix_len, 1);
        assert_eq!(inner, r"^\d+$");
        assert_eq!(quote, '"');
    }

    #[test]
    fn extract_raw_single_quote() {
        let (prefix_len, inner, quote) = extract_pattern_parts(r"r'\w+'").unwrap();
        assert_eq!(prefix_len, 1);
        assert_eq!(inner, r"\w+");
        assert_eq!(quote, '\'');
    }

    #[test]
    fn extract_uppercase_r_prefix() {
        let (prefix_len, inner, _) = extract_pattern_parts(r#"R"hello""#).unwrap();
        assert_eq!(prefix_len, 1);
        assert_eq!(inner, "hello");
    }

    #[test]
    fn reject_non_raw_string() {
        assert!(extract_pattern_parts(r#""\\d+""#).is_none());
    }

    #[test]
    fn reject_triple_quoted() {
        assert!(extract_pattern_parts(r#"r"""triple""""#).is_none());
    }

    #[test]
    fn reject_triple_single_quoted() {
        assert!(extract_pattern_parts(r"r'''triple'''").is_none());
    }

    #[test]
    fn reject_byte_string() {
        assert!(extract_pattern_parts(r#"rb"\x00""#).is_none());
    }

    #[test]
    fn reject_br_byte_string() {
        assert!(extract_pattern_parts(r#"br"\x00""#).is_none());
    }

    #[test]
    fn extract_empty_pattern() {
        let (prefix_len, inner, _) = extract_pattern_parts(r#"r"""#).unwrap();
        assert_eq!(prefix_len, 1);
        assert_eq!(inner, "");
    }

    // ── maybe_push_unique ──

    #[test]
    fn dedup_same_span_and_operator() {
        let mut mutations = vec![make_mutation("^", "", "regex_anchor_removal", 10)];
        maybe_push_unique(&mut mutations, make_mutation("^", "", "regex_anchor_removal", 10));
        assert_eq!(mutations.len(), 1);
    }

    #[test]
    fn allow_different_operator_same_span() {
        let mut mutations = vec![make_mutation("^", "", "regex_anchor_removal", 10)];
        maybe_push_unique(&mut mutations, make_mutation("^", "x", "some_other_op", 10));
        assert_eq!(mutations.len(), 2);
    }

    // ── collect_regex_mutations (empty / non-raw) ──

    #[test]
    fn non_raw_string_returns_no_mutations() {
        let mutations = collect_regex_mutations(r#""\\d+""#, 0);
        assert!(mutations.is_empty());
    }

    #[test]
    fn empty_pattern_returns_no_mutations() {
        let mutations = collect_regex_mutations(r#"r"""#, 0);
        assert!(mutations.is_empty());
    }

    // ── Helper to check span invariant ──

    fn check_span_invariant(mutations: &[Mutation]) {
        for m in mutations {
            assert!(m.start < m.end, "start ({}) must be < end ({})", m.start, m.end);
            assert_eq!(
                m.start + m.original.len(),
                m.end,
                "start + original.len() must == end for operator={}, original={:?}",
                m.operator,
                m.original
            );
        }
    }

    fn by_op<'a>(mutations: &'a [Mutation], op: &str) -> Vec<&'a Mutation> {
        mutations.iter().filter(|m| m.operator == op).collect()
    }

    // ── Anchor removal ──

    #[test]
    fn anchor_removal_start_and_end() {
        let mutations = collect_regex_mutations(r#"r"^\d+$""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].original, "^");
        assert_eq!(anchors[0].replacement, "");
        assert_eq!(anchors[1].original, "$");
        assert_eq!(anchors[1].replacement, "");
        check_span_invariant(&mutations);
    }

    #[test]
    fn anchor_removal_start_text() {
        let mutations = collect_regex_mutations(r#"r"\A\d+\z""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].original, r"\A");
        assert_eq!(anchors[1].original, r"\z");
    }

    #[test]
    fn no_anchors_no_anchor_mutations() {
        let mutations = collect_regex_mutations(r#"r"\d+""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert!(anchors.is_empty());
    }

    // ── Boundary removal ──

    #[test]
    fn boundary_removal() {
        let mutations = collect_regex_mutations(r#"r"\bfoo\b""#, 0);
        let bounds = by_op(&mutations, "regex_boundary_removal");
        assert_eq!(bounds.len(), 2);
        assert_eq!(bounds[0].original, r"\b");
        assert_eq!(bounds[0].replacement, "");
        check_span_invariant(&mutations);
    }

    #[test]
    fn not_word_boundary_removal() {
        let mutations = collect_regex_mutations(r#"r"\B""#, 0);
        let bounds = by_op(&mutations, "regex_boundary_removal");
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].original, r"\B");
    }

    // ── Shorthand negation ──

    #[test]
    fn shorthand_negation_digit() {
        let mutations = collect_regex_mutations(r#"r"\d""#, 0);
        let negs = by_op(&mutations, "regex_shorthand_negation");
        assert_eq!(negs.len(), 1);
        assert_eq!(negs[0].original, r"\d");
        assert_eq!(negs[0].replacement, r"\D");
        check_span_invariant(&mutations);
    }

    #[test]
    fn shorthand_negation_all_three() {
        let mutations = collect_regex_mutations(r#"r"\d\w\s""#, 0);
        let negs = by_op(&mutations, "regex_shorthand_negation");
        assert_eq!(negs.len(), 3);
        assert_eq!(negs[0].replacement, r"\D");
        assert_eq!(negs[1].replacement, r"\W");
        assert_eq!(negs[2].replacement, r"\S");
    }

    #[test]
    fn shorthand_negation_already_negated() {
        let mutations = collect_regex_mutations(r#"r"\D""#, 0);
        let negs = by_op(&mutations, "regex_shorthand_negation");
        assert_eq!(negs.len(), 1);
        assert_eq!(negs[0].replacement, r"\d");
    }

    // ── Charclass negation ──

    #[test]
    fn charclass_negation_add() {
        let mutations = collect_regex_mutations(r#"r"[abc]""#, 0);
        let negs = by_op(&mutations, "regex_charclass_negation");
        assert_eq!(negs.len(), 1);
        assert_eq!(negs[0].original, "[abc]");
        assert_eq!(negs[0].replacement, "[^abc]");
        check_span_invariant(&mutations);
    }

    #[test]
    fn charclass_negation_remove() {
        let mutations = collect_regex_mutations(r#"r"[^abc]""#, 0);
        let negs = by_op(&mutations, "regex_charclass_negation");
        assert_eq!(negs.len(), 1);
        assert_eq!(negs[0].original, "[^abc]");
        assert_eq!(negs[0].replacement, "[abc]");
    }

    #[test]
    fn charclass_with_range() {
        let mutations = collect_regex_mutations(r#"r"[a-z]""#, 0);
        let negs = by_op(&mutations, "regex_charclass_negation");
        assert_eq!(negs.len(), 1);
        assert_eq!(negs[0].replacement, "[^a-z]");
    }

    // ── Quantifier removal ──

    #[test]
    fn quantifier_removal_plus() {
        let mutations = collect_regex_mutations(r#"r"\d+""#, 0);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 1);
        assert_eq!(quants[0].original, r"\d+");
        assert_eq!(quants[0].replacement, r"\d");
        check_span_invariant(&mutations);
    }

    #[test]
    fn quantifier_removal_star() {
        let mutations = collect_regex_mutations(r#"r"\d*""#, 0);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 1);
        assert_eq!(quants[0].replacement, r"\d");
    }

    #[test]
    fn quantifier_removal_question() {
        let mutations = collect_regex_mutations(r#"r"\d?""#, 0);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 1);
        assert_eq!(quants[0].replacement, r"\d");
    }

    #[test]
    fn quantifier_removal_range() {
        let mutations = collect_regex_mutations(r#"r"\d{2,5}""#, 0);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 1);
        assert_eq!(quants[0].replacement, r"\d");
    }

    #[test]
    fn quantifier_removal_nested() {
        // (\d+)+ has both inner \d+ and outer (...)+ quantifiers
        let mutations = collect_regex_mutations(r#"r"(\d+)+""#, 0);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 2);
    }

    // ── Dot to charclass ──

    #[test]
    fn dot_to_charclass() {
        let mutations = collect_regex_mutations(r#"r".""#, 0);
        let dots = by_op(&mutations, "regex_dot_to_charclass");
        assert_eq!(dots.len(), 1);
        assert_eq!(dots[0].original, ".");
        assert_eq!(dots[0].replacement, r"\w");
        check_span_invariant(&mutations);
    }

    #[test]
    fn dot_in_pattern() {
        let mutations = collect_regex_mutations(r#"r"a.b""#, 0);
        let dots = by_op(&mutations, "regex_dot_to_charclass");
        assert_eq!(dots.len(), 1);
    }

    // ── Offset correctness ──

    #[test]
    fn offsets_with_nonzero_node_start() {
        // Simulate string node at byte offset 42 within the function
        let mutations = collect_regex_mutations(r#"r"^\d+$""#, 42);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        // r" prefix = 1 byte, " = 1 byte, so inner starts at 42 + 2 = 44
        // ^ is at offset 0 in pattern, so mutation.start = 44
        assert_eq!(anchors[0].start, 44);
        // $ is at offset 4 in pattern ^\d+$, so mutation.start = 44 + 4 = 48
        assert_eq!(anchors[1].start, 48);
        check_span_invariant(&mutations);
    }

    #[test]
    fn single_literal_no_mutations() {
        // A bare literal like r"a" has no quantifiers, classes, etc.
        let mutations = collect_regex_mutations(r#"r"a""#, 0);
        assert!(mutations.is_empty());
    }

    #[test]
    fn anchor_only_pattern() {
        let mutations = collect_regex_mutations(r#"r"^$""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert_eq!(anchors.len(), 2);
        check_span_invariant(&mutations);
    }

    // ── Text-based fallback: lookaround negation ──

    #[test]
    fn lookaround_positive_to_negative() {
        let mutations = collect_regex_mutations(r#"r"(?=foo)bar""#, 0);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].original, "(?=");
        assert_eq!(looks[0].replacement, "(?!");
        check_span_invariant(&mutations);
    }

    #[test]
    fn lookaround_negative_to_positive() {
        let mutations = collect_regex_mutations(r#"r"(?!foo)bar""#, 0);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].replacement, "(?=");
    }

    #[test]
    fn lookbehind_positive_to_negative() {
        let mutations = collect_regex_mutations(r#"r"(?<=foo)bar""#, 0);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].original, "(?<=");
        assert_eq!(looks[0].replacement, "(?<!");
    }

    #[test]
    fn lookbehind_negative_to_positive() {
        let mutations = collect_regex_mutations(r#"r"(?<!foo)bar""#, 0);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].replacement, "(?<=");
    }

    #[test]
    fn multiple_lookarounds() {
        let mutations = collect_regex_mutations(r#"r"(?=a)(?!b)(?<=c)(?<!d)x""#, 0);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 4);
    }

    // ── Text-based fallback: dedup with AST ──

    #[test]
    fn text_fallback_dedup_with_ast() {
        // AST produces anchor removal for ^ and $; text-based should not duplicate them.
        let mutations = collect_regex_mutations(r#"r"^\d+$""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert_eq!(anchors.len(), 2); // Exactly 2, not 4.
    }

    #[test]
    fn text_fallback_dedup_shorthand_with_ast() {
        let mutations = collect_regex_mutations(r#"r"\d""#, 0);
        let negs = by_op(&mutations, "regex_shorthand_negation");
        assert_eq!(negs.len(), 1); // Not 2.
    }

    // ── Text-based fallback on unparseable pattern ──

    #[test]
    fn text_fallback_on_lookahead_pattern() {
        // regex-syntax fails on (?=...), but text-based picks up shorthand + lookaround
        let mutations = collect_regex_mutations(r#"r"(?=foo)\d+""#, 0);
        let negs = by_op(&mutations, "regex_shorthand_negation");
        assert_eq!(negs.len(), 1);
        let looks = by_op(&mutations, "regex_lookaround_negation");
        assert_eq!(looks.len(), 1);
        check_span_invariant(&mutations);
    }

    #[test]
    fn escaped_dollar_not_anchor() {
        // \$ at end should not trigger anchor removal
        let mutations = collect_regex_mutations(r#"r"\d+\$""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert!(anchors.is_empty());
    }

    // ── Tier 2: Quantifier range changes ──

    #[test]
    fn quantifier_change_bounded() {
        let mutations = collect_regex_mutations(r#"r"\d{2,5}""#, 0);
        let changes = by_op(&mutations, "regex_quantifier_change");
        // {2,5} → {1,5}, {3,5}, {2,4}, {2,6} = 4 variants
        assert_eq!(changes.len(), 4, "expected 4 range variants, got {:?}", changes);
        check_span_invariant(&mutations);
    }

    #[test]
    fn quantifier_change_at_least() {
        let mutations = collect_regex_mutations(r#"r"\d{2,}""#, 0);
        let changes = by_op(&mutations, "regex_quantifier_change");
        // {2,} → {1,}, {3,} = 2 variants
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn quantifier_change_exactly() {
        let mutations = collect_regex_mutations(r#"r"\d{3}""#, 0);
        let changes = by_op(&mutations, "regex_quantifier_change");
        // {3} → {2}, {4} = 2 variants
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn quantifier_change_zero_lower_no_underflow() {
        let mutations = collect_regex_mutations(r#"r"\d{0,5}""#, 0);
        let changes = by_op(&mutations, "regex_quantifier_change");
        // {0,5} → no {-1,5}, {1,5}, {0,4}, {0,6} = 3 variants
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn quantifier_change_zero_exactly_no_underflow() {
        let mutations = collect_regex_mutations(r#"r"\d{0}""#, 0);
        let changes = by_op(&mutations, "regex_quantifier_change");
        // {0} → no {-1}, just {1} = 1 variant
        assert_eq!(changes.len(), 1);
    }

    // ── Tier 2: Character class child removal ──

    #[test]
    fn charclass_child_removal_three_items() {
        let mutations = collect_regex_mutations(r#"r"[abc]""#, 0);
        let removals = by_op(&mutations, "regex_charclass_child_removal");
        assert_eq!(removals.len(), 3); // [bc], [ac], [ab]
        assert!(removals.iter().any(|m| m.replacement == "[bc]"));
        assert!(removals.iter().any(|m| m.replacement == "[ac]"));
        assert!(removals.iter().any(|m| m.replacement == "[ab]"));
        check_span_invariant(&mutations);
    }

    #[test]
    fn charclass_child_removal_range() {
        let mutations = collect_regex_mutations(r#"r"[a-z0-9]""#, 0);
        let removals = by_op(&mutations, "regex_charclass_child_removal");
        assert_eq!(removals.len(), 2); // [0-9], [a-z]
        check_span_invariant(&mutations);
    }

    #[test]
    fn charclass_child_removal_single_item() {
        let mutations = collect_regex_mutations(r#"r"[a]""#, 0);
        let removals = by_op(&mutations, "regex_charclass_child_removal");
        assert!(removals.is_empty(), "single-item class should not get child removal");
    }

    #[test]
    fn charclass_child_removal_negated() {
        let mutations = collect_regex_mutations(r#"r"[^abc]""#, 0);
        let removals = by_op(&mutations, "regex_charclass_child_removal");
        assert_eq!(removals.len(), 3);
        // Should preserve the ^ negation
        assert!(removals.iter().all(|m| m.replacement.starts_with("[^")));
    }

    // ── Tier 2: Group to non-capturing ──

    #[test]
    fn group_to_noncapturing() {
        let mutations = collect_regex_mutations(r#"r"(\d+)""#, 0);
        let groups = by_op(&mutations, "regex_group_to_noncapturing");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].original, r"(\d+)");
        assert_eq!(groups[0].replacement, r"(?:\d+)");
        check_span_invariant(&mutations);
    }

    #[test]
    fn group_noncapturing_not_mutated() {
        let mutations = collect_regex_mutations(r#"r"(?:\d+)""#, 0);
        let groups = by_op(&mutations, "regex_group_to_noncapturing");
        assert!(groups.is_empty());
    }

    // ── Tier 2: Alternation removal ──

    #[test]
    fn alternation_removal_three_branches() {
        let mutations = collect_regex_mutations(r#"r"cat|dog|bird""#, 0);
        let alts = by_op(&mutations, "regex_alternation_removal");
        assert_eq!(alts.len(), 3);
        assert!(alts.iter().any(|m| m.replacement == "dog|bird"));
        assert!(alts.iter().any(|m| m.replacement == "cat|bird"));
        assert!(alts.iter().any(|m| m.replacement == "cat|dog"));
        check_span_invariant(&mutations);
    }

    #[test]
    fn alternation_removal_two_branches() {
        let mutations = collect_regex_mutations(r#"r"yes|no""#, 0);
        let alts = by_op(&mutations, "regex_alternation_removal");
        assert_eq!(alts.len(), 2);
        assert!(alts.iter().any(|m| m.replacement == "no"));
        assert!(alts.iter().any(|m| m.replacement == "yes"));
    }

    #[test]
    fn no_alternation_single_pattern() {
        let mutations = collect_regex_mutations(r#"r"single""#, 0);
        let alts = by_op(&mutations, "regex_alternation_removal");
        assert!(alts.is_empty());
    }

    // ── Edge cases ──

    #[test]
    fn nested_quantifier_in_group() {
        // (\d+)+ has both inner \d+ and outer (...)+ quantifiers, plus group_to_noncapturing
        let mutations = collect_regex_mutations(r#"r"(\d+)+""#, 0);
        let quant_removals = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quant_removals.len(), 2);
        let groups = by_op(&mutations, "regex_group_to_noncapturing");
        assert_eq!(groups.len(), 1);
        check_span_invariant(&mutations);
    }

    #[test]
    fn quantifier_on_group_with_range() {
        let mutations = collect_regex_mutations(r#"r"(?:abc){2,4}""#, 0);
        let quant_removals = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quant_removals.len(), 1);
        let quant_changes = by_op(&mutations, "regex_quantifier_change");
        assert!(quant_changes.len() >= 3);
        // Non-capturing group should NOT get group_to_noncapturing
        let groups = by_op(&mutations, "regex_group_to_noncapturing");
        assert!(groups.is_empty());
    }

    #[test]
    fn complex_email_pattern() {
        // Realistic pattern exercises multiple operators
        let mutations = collect_regex_mutations(r#"r"^[^@]+@[^@]+\.[^@]+$""#, 0);
        let anchors = by_op(&mutations, "regex_anchor_removal");
        assert_eq!(anchors.len(), 2);
        let negs = by_op(&mutations, "regex_charclass_negation");
        assert!(negs.len() >= 3);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert!(quants.len() >= 3);
        check_span_invariant(&mutations);
    }

    #[test]
    fn dot_star_pattern() {
        let mutations = collect_regex_mutations(r#"r".*""#, 0);
        let dots = by_op(&mutations, "regex_dot_to_charclass");
        assert_eq!(dots.len(), 1);
        let quants = by_op(&mutations, "regex_quantifier_removal");
        assert_eq!(quants.len(), 1);
    }

    #[test]
    fn all_operators_on_rich_pattern() {
        // Pattern with anchors, charclass, shorthand, quantifier, dot, group, alternation, boundary
        let mutations = collect_regex_mutations(r#"r"^\b(\d+|[abc]).?\w*$""#, 0);
        // Just verify it doesn't panic and produces mutations
        assert!(!mutations.is_empty());
        check_span_invariant(&mutations);
        // Verify diverse operator set
        let ops: std::collections::HashSet<&str> = mutations.iter().map(|m| m.operator).collect();
        assert!(ops.contains("regex_anchor_removal"));
        assert!(ops.contains("regex_shorthand_negation"));
        assert!(ops.contains("regex_quantifier_removal"));
    }
}
