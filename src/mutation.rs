//! Mutation engine: parse Python source, identify mutation points, generate mutant variants.
//!
//! Delegates to the tree-sitter-based collector in `tree_sitter_mutation.rs`.
//! Byte spans come directly from the parser — no monotonic cursor hack needed.
//! This module owns the shared types (`Mutation`, `FunctionMutations`) and `apply_mutation`.

/// A single mutation that can be applied to source code.
#[derive(Debug, Clone)]
pub struct Mutation {
    /// Byte offset in the function source where the original text starts.
    pub start: usize,
    /// Byte offset one past the end of the original text.
    pub end: usize,
    /// The original text to replace.
    pub original: String,
    /// The replacement text.
    pub replacement: String,
    /// Which operator produced this mutation.
    pub operator: &'static str,
}

/// Descriptor decorators that irradiate can trampoline through.
///
/// These three stdlib decorators only change the calling convention — they have
/// no definition-time side effects and their semantics are completely predictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorDecorator {
    Property,
    ClassMethod,
    StaticMethod,
}

/// Information about a function and its mutations.
#[derive(Debug, Clone)]
pub struct FunctionMutations {
    /// Function name as it appears in the source.
    pub name: String,
    /// Class name if this is a method.
    pub class_name: Option<String>,
    /// The complete source text of the function definition.
    pub source: String,
    /// The function's parameter list source text (for trampoline wrapper).
    pub params_source: String,
    /// Return type annotation text, e.g. " -> int | None". Empty if none.
    pub return_annotation: String,
    /// Whether the function is async.
    pub is_async: bool,
    /// Whether the function is a generator (contains `yield` at the function body level,
    /// not inside nested functions). An async generator has both `is_async` and `is_generator`.
    pub is_generator: bool,
    /// Mutations found within this function body.
    pub mutations: Vec<Mutation>,
    /// 1-indexed start line of the function in the source file.
    pub start_line: usize,
    /// 1-indexed end line of the function in the source file.
    pub end_line: usize,
    /// Byte offset of the function definition start in the source file.
    /// Combined with `Mutation.start` (byte offset within the function source),
    /// gives the absolute byte position in the file: `byte_offset + mutation.start`.
    pub byte_offset: usize,
    /// If this function has a descriptor decorator (@property, @classmethod, @staticmethod),
    /// store which kind so the trampoline can generate the correct wrapper.
    pub descriptor_decorator: Option<DescriptorDecorator>,
}

/// Collect all function mutations from a Python source file.
///
/// Delegates to the tree-sitter-based collector, which uses byte spans directly
/// from the parser — no monotonic cursor hack needed.
pub fn collect_file_mutations(source: &str) -> Vec<FunctionMutations> {
    crate::tree_sitter_mutation::collect_file_mutations_tree_sitter(source)
}

/// Apply a single mutation to a function's source text.
pub fn apply_mutation(func_source: &str, mutation: &Mutation) -> String {
    format!(
        "{}{}{}",
        &func_source[..mutation.start],
        mutation.replacement,
        &func_source[mutation.end..]
    )
}

// Used by many test modules for parse-validity assertions.
#[cfg(test)]
use libcst_native::parse_module;

/// Return all mutations from `source` whose operator equals `operator`.
///
/// Convenience for test modules that need to filter by operator without
/// repeating the flat-map + filter chain everywhere.
#[cfg(test)]
fn mutations_by_operator(source: &str, operator: &str) -> Vec<Mutation> {
    collect_file_mutations(source)
        .into_iter()
        .flat_map(|fm| fm.mutations)
        .filter(|m| m.operator == operator)
        .collect()
}

/// Assert that the byte slice `fm.source[m.start..m.end]` equals `m.original`.
///
/// Use instead of the inline `assert_eq!(&fm.source[m.start..m.end], m.original.as_str(), …)`
/// to keep span-validity checks uniform and reduce noise.
#[cfg(test)]
fn assert_span_matches_original(fm: &FunctionMutations, m: &Mutation) {
    assert_eq!(&fm.source[m.start..m.end], m.original.as_str(), "span must match original");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `"def add(a, b):\n    return a + b\n"` — the minimal two-argument function used
    /// across many tests that need a function with a binary-operator mutation point.
    const SIMPLE_ADD: &str = "def add(a, b):\n    return a + b\n";

    #[test]
    fn test_function_byte_offset_at_file_start() {
        // A single function at byte 0: byte_offset must be 0.
        let source = "def foo(x):\n    return x + 1\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        assert_eq!(fms[0].byte_offset, 0, "function at top of file must have byte_offset 0");
    }

    #[test]
    fn test_function_byte_offset_not_at_file_start() {
        // Function preceded by module-level code.
        let source = "X = 1\n\ndef foo(x):\n    return x + 1\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let expected = source.find("def foo").unwrap();
        assert_eq!(
            fms[0].byte_offset, expected,
            "byte_offset must point at the 'd' of 'def'"
        );
    }

    #[test]
    fn test_multiple_functions_have_distinct_byte_offsets() {
        let source = "def a(x):\n    return x + 1\n\ndef b(x):\n    return x - 1\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 2);
        let offset_a = source.find("def a").unwrap();
        let offset_b = source.find("def b").unwrap();
        let fm_a = fms.iter().find(|f| f.name == "a").unwrap();
        let fm_b = fms.iter().find(|f| f.name == "b").unwrap();
        assert_eq!(fm_a.byte_offset, offset_a);
        assert_eq!(fm_b.byte_offset, offset_b);
    }

    #[test]
    fn test_collect_binop_mutations() {
        let source = SIMPLE_ADD;
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.name, "add");

        let binop_mutations: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert!(!binop_mutations.is_empty(), "Should find + → - mutation");
        assert!(
            binop_mutations[0].replacement.contains('-'),
            "Should swap + to -"
        );
    }

    #[test]
    fn test_collect_comparison_mutations() {
        let source = "def check(n):\n    if n > 0:\n        return True\n    return False\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let compop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "compop_swap");
        assert!(compop.is_some(), "Should find > → >= mutation");

        let name_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "name_swap")
            .collect();
        assert!(
            name_muts.len() >= 2,
            "Should find True→False and False→True"
        );
    }

    #[test]
    fn test_collect_string_mutations() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let string_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "string_mutation");
        assert!(string_mut.is_some(), "Should find string mutation");
        assert!(
            string_mut.unwrap().replacement.contains("XX"),
            "Should add XX prefix/suffix"
        );
    }

    #[test]
    fn test_decorated_functions_are_skipped() {
        // All decorated functions are skipped — matches mutmut's blanket skip behavior.
        let source = "@decorator\ndef foo():\n    return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "Decorated function must not be collected");
    }

    #[test]
    fn test_skip_docstrings() {
        let source = "def foo():\n    \"\"\"docstring\"\"\"\n    return 1\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let string_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "string_mutation")
            .collect();
        assert!(string_muts.is_empty(), "Docstrings should not be mutated");
    }

    #[test]
    fn test_apply_mutation() {
        let source = SIMPLE_ADD;
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let binop = fm
            .mutations
            .iter()
            .find(|m| m.operator == "binop_swap")
            .unwrap();

        let mutated = apply_mutation(&fm.source, binop);
        assert!(mutated.contains(" - "), "Should have - instead of +");
        assert!(!mutated.contains(" + "), "Should not have + anymore");
    }

    #[test]
    fn test_number_mutation() {
        let source = "def foo():\n    return 42\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let num_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "number_mutation");
        assert!(num_mut.is_some());
        assert_eq!(num_mut.unwrap().replacement, "43");
    }

    #[test]
    fn test_boolean_op_mutation() {
        let source = "def foo(a, b):\n    return a and b\n";
        let fms = collect_file_mutations(source);
        let boolop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "boolop_swap");
        assert!(boolop.is_some(), "Should find and → or mutation");
    }

    #[test]
    fn test_pragma_no_mutate() {
        // Entire body is on a pragma line — all mutations suppressed, function omitted.
        let source = "def foo():\n    return 1 + 2  # pragma: no mutate\n";
        let fms = collect_file_mutations(source);
        assert!(
            fms.is_empty(),
            "All mutations suppressed → function should be omitted"
        );
    }

    #[test]
    fn test_pragma_blocks_binop() {
        let source = "def foo(a, b):\n    return a + b  # pragma: no mutate\n";
        let fms = collect_file_mutations(source);
        let binops: Vec<_> = fms
            .first()
            .map(|f| {
                f.mutations
                    .iter()
                    .filter(|m| m.operator == "binop_swap")
                    .collect()
            })
            .unwrap_or_default();
        assert!(binops.is_empty(), "Pragma should block + → - mutation");
    }

    #[test]
    fn test_pragma_selective() {
        // Line 3 uses `*` (unique token on pragma line) to avoid cursor-offset ambiguity.
        // Lines 2 and 4 have `+` and should each produce one binop mutation.
        let source =
            "def foo(a, b, c):\n    x = a + b\n    y = b * c  # pragma: no mutate\n    return x + y\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "Function should still be collected");
        let binops: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        // Lines 2 (+) and 4 (+) produce mutations; line 3 (*) is suppressed entirely.
        assert_eq!(
            binops.len(),
            2,
            "Should have mutations for lines 2 and 4, but not the pragma line 3"
        );
    }

    #[test]
    fn test_pragma_whole_line_all_operators() {
        // A line with both a binop and a comparison — pragma suppresses all of them.
        let source = "def foo(a, b):\n    x = 1 + 2  # pragma: no mutate\n    return a > b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "Non-pragma line still produces mutations");
        // Number/binop mutations from line 2 should be gone; compop from line 3 remains.
        let line2_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "number_mutation" || m.operator == "binop_swap")
            .collect();
        assert!(
            line2_muts.is_empty(),
            "Pragma suppresses all operators on that line"
        );
        let compop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "compop_swap");
        assert!(compop.is_some(), "Non-pragma lines are unaffected");
    }

    #[test]
    fn test_class_methods() {
        let source = "class Foo:\n    def bar(self):\n        return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        assert_eq!(fms[0].name, "bar");
        assert_eq!(fms[0].class_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn test_lambda_mutation() {
        let source = "def foo():\n    f = lambda x: x + 1\n";
        let fms = collect_file_mutations(source);
        let lam = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "lambda_mutation");
        assert!(lam.is_some(), "Should find lambda → None mutation");
    }

    #[test]
    fn test_method_swap_lower_upper() {
        let source = "def foo(s):\n    return s.lower()\n";
        let fms = collect_file_mutations(source);
        let method_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap");
        assert!(
            method_mut.is_some(),
            "Should find .lower() → .upper() mutation"
        );
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lower");
        assert_eq!(m.replacement, "upper");
    }

    #[test]
    fn test_method_swap_lstrip_rstrip() {
        let source = "def foo(s):\n    return s.lstrip()\n";
        let fms = collect_file_mutations(source);
        let method_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap");
        assert!(method_mut.is_some());
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lstrip");
        assert_eq!(m.replacement, "rstrip");
    }

    #[test]
    fn test_method_swap_ljust_rjust() {
        let source = "def foo(s):\n    return s.ljust(10)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "ljust");
        assert_eq!(m.replacement, "rjust");
    }

    #[test]
    fn test_method_swap_rjust_ljust() {
        let source = "def foo(s):\n    return s.rjust(10)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rjust");
        assert_eq!(m.replacement, "ljust");
    }

    #[test]
    fn test_method_swap_index_rindex() {
        let source = "def foo(s):\n    return s.index('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "index");
        assert_eq!(m.replacement, "rindex");
    }

    #[test]
    fn test_method_swap_rindex_index() {
        let source = "def foo(s):\n    return s.rindex('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rindex");
        assert_eq!(m.replacement, "index");
    }

    #[test]
    fn test_method_swap_removeprefix_removesuffix() {
        let source = "def foo(s):\n    return s.removeprefix('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "removeprefix");
        assert_eq!(m.replacement, "removesuffix");
    }

    #[test]
    fn test_method_swap_removesuffix_removeprefix() {
        let source = "def foo(s):\n    return s.removesuffix('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "removesuffix");
        assert_eq!(m.replacement, "removeprefix");
    }

    #[test]
    fn test_method_swap_partition_rpartition() {
        let source = "def foo(s):\n    return s.partition('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "partition");
        assert_eq!(m.replacement, "rpartition");
    }

    #[test]
    fn test_method_swap_rpartition_partition() {
        let source = "def foo(s):\n    return s.rpartition('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rpartition");
        assert_eq!(m.replacement, "partition");
    }

    #[test]
    fn test_chained_method_swaps() {
        let source = "def foo(s):\n    return s.lower().lstrip()\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert_eq!(method_muts.len(), 2, "Should find 2 method swap mutations");
    }

    // INV-1: When the object variable name equals the method name, the mutation span
    // must cover the method (after the dot), NOT the object name (before the dot).
    #[test]
    fn test_method_swap_object_name_equals_method_name() {
        let source = "def foo(s):\n    return find.find('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("Should find method_swap mutation on find.find()");

        assert_eq!(m.original, "find");
        assert_eq!(m.replacement, "rfind");

        // The span must cover exactly the method name text.
        let span_text = &fms[0].source[m.start..m.end];
        assert_eq!(span_text, "find", "Span should cover the method name, not the object");

        // The character immediately before the method start must be a dot.
        assert_eq!(
            &fms[0].source[m.start - 1..m.start],
            ".",
            "Character before method span start must be a dot"
        );
    }

    // INV-3: For any method_swap mutation m, source[m.start..m.end] equals the original name.
    // Also validates that the character before the span is always a dot (structural guarantee).
    #[test]
    fn test_method_swap_span_structural_correctness() {
        let cases = [
            "def foo(s):\n    return s.lower()\n",
            "def foo(s):\n    return s.upper()\n",
            "def foo(s):\n    return find.find('x')\n",
            "def foo(s):\n    return lower.lower()\n",
            "def foo(s):\n    return s.strip().lower()\n",
            "def foo(s):\n    return upper.upper()\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    if m.operator == "method_swap" {
                        let span_text = &fm.source[m.start..m.end];
                        assert_eq!(
                            span_text, m.original,
                            "INV-3: span [{}, {}) = {:?} but original = {:?} in {:?}",
                            m.start, m.end, span_text, m.original, source
                        );
                        // Structural guarantee: immediately before the method name is always a dot.
                        assert_eq!(
                            &fm.source[m.start - 1..m.start],
                            ".",
                            "Character before method span must be a dot, violated in {:?}",
                            source
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_non_matching_method_not_mutated() {
        let source = "def foo(s):\n    return s.strip()\n";
        let fms = collect_file_mutations(source);
        // No mutations at all means no method_swap mutations — the function is excluded entirely
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), ".strip() is not in METHOD_SWAPS");
    }

    // INV-2: string content = delimiter char must not produce syntactically invalid Python
    #[test]
    fn test_string_mutation_double_quote_in_single_quoted() {
        // '"' is a single-quoted string whose content is a double-quote.
        // Before the fix, quote_char was incorrectly detected as '"', producing
        // the invalid replacement '"XXXX" (unterminated single-quoted string).
        // After the fix, quote_char = '\'', producing 'XX"XX' (valid Python).
        let source = "def foo(s):\n    return s.replace('\"', 'x')\n";
        let fms = collect_file_mutations(source);
        if let Some(fm) = fms.first() {
            for m in fm
                .mutations
                .iter()
                .filter(|m| m.operator == "string_mutation")
            {
                // The replacement must be a valid Python string literal.
                // For '"', it must be delimited by single-quotes: starts with ' ends with '
                if m.original == "'\"'" {
                    assert!(
                        m.replacement.starts_with('\'') && m.replacement.ends_with('\''),
                        "Replacement for '\"' must stay single-quoted, got: {}",
                        m.replacement
                    );
                    // Must not start with '"' (which would produce an unterminated string)
                    assert!(
                        !m.replacement.starts_with("'\""),
                        "Replacement must not produce unterminated string, got: {}",
                        m.replacement
                    );
                }
            }
        }
    }

    // INV-2: single-quote inside double-quoted string must also produce valid Python
    #[test]
    fn test_string_mutation_single_quote_in_double_quoted() {
        // "'" is a double-quoted string whose content is a single-quote.
        let source = "def foo(s):\n    return s.replace(\"'\", 'x')\n";
        let fms = collect_file_mutations(source);
        if let Some(fm) = fms.first() {
            for m in fm
                .mutations
                .iter()
                .filter(|m| m.operator == "string_mutation")
            {
                if m.original == "\"'\"" {
                    // Must be delimited by double-quotes
                    assert!(
                        m.replacement.starts_with('"') && m.replacement.ends_with('"'),
                        "Replacement for \"'\" must stay double-quoted, got: {}",
                        m.replacement
                    );
                }
            }
        }
    }

    // INV-3: Normal string mutations must still work after the delimiter-char fix.
    #[test]
    fn test_string_mutation_normal_strings_unaffected() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        let string_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "string_mutation");
        assert!(string_mut.is_some(), "Normal string must still be mutated");
        assert_eq!(
            string_mut.unwrap().replacement,
            "\"XXhelloXX\"",
            "Normal string mutation should produce XXhelloXX"
        );
    }

    // INV-1: Applying string mutation to a delimiter-char string must produce parseable Python.
    // Regression test for the markupsafe case: replace('"', "&#34;") where '"' is a
    // single-quoted string whose content IS the double-quote delimiter character.
    // Before the fix, the generated mutant '"XXXX" was an unterminated string → SyntaxError.
    #[test]
    fn test_string_mutation_delimiter_char_produces_parseable_python() {
        // Mirrors markupsafe's _native.py: .replace('"', "&#34;")
        let source = "def escape(s):\n    return s.replace('\"', '&#34;')\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations from escape()");
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "string_mutation")
        {
            let mutated_func = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated_func, None).is_ok(),
                "Mutating '{}' → '{}' produced unparseable Python:\n{}",
                m.original,
                m.replacement,
                mutated_func
            );
        }
    }

    // --- Generator detection tests ---
    //
    // Note: the mutation engine only collects mutations for specific operators
    // (comparison, binop, boolop, number, string, etc.). Generator functions must
    // contain at least one such mutation to be collected. We use `if n > 0:` for
    // comparisons, which guarantees a compop mutation.

    // INV-1: A function with `yield` at the top level is a generator.
    #[test]
    fn test_generator_function_is_detected() {
        // `n > 0` produces a compop mutation, so the function is collected.
        let source = "def gen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations (compop on n > 0)");
        assert!(
            fms[0].is_generator,
            "function with yield should be is_generator=true"
        );
        assert!(!fms[0].is_async, "plain generator is not async");
    }

    // INV-2: An async function with `yield` is an async generator.
    #[test]
    fn test_async_generator_function_is_detected() {
        let source = "async def agen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations (compop on n > 0)");
        assert!(
            fms[0].is_generator,
            "async function with yield should be is_generator=true"
        );
        assert!(fms[0].is_async, "should also be is_async=true");
    }

    // INV-3: A regular function (no yield) is NOT a generator.
    #[test]
    fn test_regular_function_not_generator() {
        let source = SIMPLE_ADD;
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        assert!(
            !fms[0].is_generator,
            "regular function must not be is_generator"
        );
    }

    // INV-4: yield in a separate function does NOT affect `is_generator` of a different function.
    #[test]
    fn test_non_generator_function_is_not_generator() {
        // outer has only a binop mutation; inner (separate top-level) has yield + compop.
        let source =
            "def outer(x):\n    return x + 1\n\ndef inner(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        let outer = fms
            .iter()
            .find(|fm| fm.name == "outer")
            .expect("outer must exist");
        let inner = fms
            .iter()
            .find(|fm| fm.name == "inner")
            .expect("inner must exist");
        assert!(
            !outer.is_generator,
            "outer has no yield, must not be is_generator"
        );
        assert!(inner.is_generator, "inner has yield, must be is_generator");
    }

    // --- arg_removal operator tests ---

    fn arg_removal_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "arg_removal")
    }

    // INV-1: f(a, b) → 4 arg_removal mutations: replace each arg + remove each arg
    #[test]
    fn test_arg_removal_two_args() {
        let source = "def foo(a, b):\n    f(a, b)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            4,
            "f(a, b) must produce 4 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b)")),
            "missing f(None, b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, None)")),
            "missing f(a, None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b)")),
            "missing f(b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a)")),
            "missing f(a)"
        );
    }

    // INV-2: f(a) → 1 mutation: replace with None (no removal)
    #[test]
    fn test_arg_removal_single_arg() {
        let source = "def foo(a):\n    f(a)\n";
        let muts = arg_removal_mutations(source);
        assert_eq!(
            muts.len(),
            1,
            "f(a) must produce exactly 1 arg_removal mutation"
        );
        assert!(
            muts[0].replacement.contains("f(None)"),
            "should produce f(None)"
        );
    }

    // INV-3: f(*args) → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_star_args_skipped() {
        let source = "def foo(args):\n    f(*args)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(*args) must produce 0 arg_removal mutations"
        );
    }

    // INV-4: f(**kwargs) → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_double_star_kwargs_skipped() {
        let source = "def foo(kwargs):\n    f(**kwargs)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(**kwargs) must produce 0 arg_removal mutations"
        );
    }

    // INV-5: f(None) → 0 arg_removal mutations (already None, only arg so no removal)
    #[test]
    fn test_arg_removal_already_none_single() {
        let source = "def foo():\n    f(None)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(None) single arg must produce 0 arg_removal mutations"
        );
    }

    // INV-6: f() → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_empty_call() {
        let source = "def foo():\n    f()\n";
        let muts = arg_removal_mutations(source);
        assert!(muts.is_empty(), "f() must produce 0 arg_removal mutations");
    }

    // INV-7: f(a, b=2) handles keyword args correctly
    #[test]
    fn test_arg_removal_keyword_arg() {
        let source = "def foo(a):\n    f(a, b=2)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            4,
            "f(a, b=2) must produce 4 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b=2)")),
            "missing f(None, b=2)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b=None)")),
            "missing f(a, b=None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b=2)")),
            "missing f(b=2)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a)")),
            "missing f(a)"
        );
    }

    // Three-arg call: f(a, b, c) → 6 mutations (replace each × 3 + remove each × 3)
    #[test]
    fn test_arg_removal_three_args() {
        let source = "def foo(a, b, c):\n    f(a, b, c)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            6,
            "f(a, b, c) must produce 6 arg_removal mutations; got: {replacements:?}"
        );
        // replace mutations
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b, c)")),
            "missing f(None, b, c)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, None, c)")),
            "missing f(a, None, c)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b, None)")),
            "missing f(a, b, None)"
        );
        // removal mutations
        assert!(
            replacements.iter().any(|r| r.contains("f(b, c)")),
            "missing f(b, c) — remove first"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, c)")),
            "missing f(a, c) — remove middle"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b)")),
            "missing f(a, b) — remove last"
        );
    }

    // None arg in multi-arg call: removal is generated even though replace is skipped
    #[test]
    fn test_arg_removal_none_arg_in_multi_arg() {
        let source = "def foo(b):\n    f(None, b)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        // arg 0 (None): no replace (already None), but remove → f(b)
        // arg 1 (b): replace → f(None, None), remove → f(None)
        assert_eq!(
            muts.len(),
            3,
            "f(None, b) must produce 3 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b)")),
            "missing f(b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, None)")),
            "missing f(None, None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None)")),
            "missing f(None)"
        );
    }

    // INV-8: All generated arg_removal mutations produce syntactically valid Python
    #[test]
    fn test_arg_removal_all_mutations_parseable() {
        let source = "def foo(a, b, c):\n    result = f(a, b, c)\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations");
        for m in fm.mutations.iter().filter(|m| m.operator == "arg_removal") {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "arg_removal mutation '{}' → '{}' produced unparseable Python:\n{}",
                m.original,
                m.replacement,
                mutated
            );
        }
    }

    // Mixed: star and normal args together — only non-starred args get mutations
    #[test]
    fn test_arg_removal_mixed_star_and_normal() {
        // f(a, *args) — arg 0 is normal, arg 1 is starred
        let source = "def foo(a, args):\n    f(a, *args)\n";
        let muts = arg_removal_mutations(source);
        // arg 0 (a): replace with None (1 mutation); no removal because starred args.len()=2 BUT
        // *args is skipped, so the removal loop sees len=2 > 1 and removes arg 0 → f(*args)
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        // arg 0 produces: replace → f(None, *args), remove → f(*args)
        // arg 1 (*args): skipped entirely
        assert_eq!(
            muts.len(),
            2,
            "f(a, *args) must produce 2 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, *args)")),
            "missing f(None, *args)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(*args)")),
            "missing f(*args)"
        );
    }

    // --- annotation skip tests ---

    // INV-1: Type annotations in function parameters produce 0 mutations.
    #[test]
    fn test_annotation_skip_param_types() {
        // int and str appear in the function signature, not the body.
        // The cursor starts past the header so they are never visited.
        let source = "def f(x: int) -> str:\n    return x\n";
        let fms = collect_file_mutations(source);
        // The body only has `return x` — `x` is a Name but not True/False/deepcopy → 0 mutations.
        // Verify no mutations come from `int` or `str` in the signature.
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let ann_muts: Vec<_> = all_muts
            .iter()
            .filter(|m| m.original == "int" || m.original == "str")
            .collect();
        assert!(
            ann_muts.is_empty(),
            "type annotations must not produce mutations"
        );
    }

    // INV-2: Variable annotation (AnnAssign) produces 0 mutations on the annotation.
    #[test]
    fn test_annotation_skip_ann_assign_type() {
        // `x: int = 5` — the annotation `int` must not be mutated; the value 5 may be.
        let source = "def foo():\n    x: int = 5\n    return x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let int_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "int").collect();
        assert!(
            int_muts.is_empty(),
            "annotation 'int' must not produce mutations"
        );
        // The value 5 should produce a number mutation.
        let num_muts: Vec<_> = all_muts
            .iter()
            .filter(|m| m.operator == "number_mutation")
            .collect();
        assert!(
            !num_muts.is_empty(),
            "value '5' in annotation assignment should still be mutated"
        );
    }

    // INV-3: Pure type annotation (no value) produces 0 mutations.
    #[test]
    fn test_annotation_skip_pure_ann_assign() {
        // `x: int` with no value — nothing should be mutated.
        let source = "def foo():\n    x: int\n    return 1 + 1\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let int_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "int").collect();
        assert!(
            int_muts.is_empty(),
            "pure annotation 'x: int' must produce 0 mutations on int"
        );
    }

    // INV-4: Generic annotation like List[int] produces 0 mutations.
    #[test]
    fn test_annotation_skip_generic() {
        let source = "def foo():\n    x: list = []\n    return x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        // The annotation `list` is a Name, but should not produce mutations.
        let list_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "list").collect();
        assert!(
            list_muts.is_empty(),
            "annotation 'list' must not produce mutations"
        );
    }

    // --- NEVER_MUTATE_FUNCTION_CALLS tests ---

    // INV-5: len(x) produces 0 mutations (call and argument both skipped).
    #[test]
    fn test_len_call_not_mutated() {
        let source = "def foo(x):\n    return len(x)\n";
        let fms = collect_file_mutations(source);
        // len(x) should produce 0 call-level mutations (no arg_removal, no method_swap, x not visited).
        // return_value and statement_deletion on the enclosing return statement are acceptable.
        let call_level_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator != "return_value" && m.operator != "statement_deletion")
            .collect();
        assert!(
            call_level_muts.is_empty(),
            "len(x) must produce 0 call-level mutations, got: {:?}",
            call_level_muts
        );
    }

    // INV-6: isinstance(x, int) produces 0 call-level mutations.
    #[test]
    fn test_isinstance_call_not_mutated() {
        let source = "def foo(x):\n    return isinstance(x, int)\n";
        let fms = collect_file_mutations(source);
        // isinstance(x, int) should produce 0 call-level mutations — only return_value
        // and statement_deletion mutations on the enclosing return statement are acceptable.
        let call_level_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator != "return_value" && m.operator != "statement_deletion")
            .collect();
        assert!(
            call_level_muts.is_empty(),
            "isinstance(x, int) must produce 0 call-level mutations, got: {:?}",
            call_level_muts
        );
    }

    // INV-7: Regular calls are still mutated (len/isinstance skip is not a general rule).
    #[test]
    fn test_regular_calls_still_mutated() {
        let source = "def foo(x):\n    return list(x)\n";
        let fms = collect_file_mutations(source);
        // list(x) — arg x produces arg_removal mutation (replace with None)
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        assert!(
            !all_muts.is_empty(),
            "regular calls like list(x) must still produce mutations"
        );
    }

    // INV-8: len() inside a larger expression doesn't block other mutations.
    #[test]
    fn test_len_inside_expression_doesnt_block_other_muts() {
        // a + len(x): the + should still produce a binop_swap mutation.
        let source = "def foo(a, x):\n    return a + len(x)\n";
        let fms = collect_file_mutations(source);
        let binops: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert!(
            !binops.is_empty(),
            "binop + should still produce a mutation even when len() is present"
        );
    }
}

#[cfg(test)]
mod offset_correctness_tests {
    use super::*;

    // INV-1: a + b + c produces 2 independent mutations, each applied correctly
    #[test]
    fn test_duplicate_operators_independent_mutations() {
        let source = "def foo(a, b, c):\n    return a + b + c\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert_eq!(binops.len(), 2, "Should find exactly 2 + operators");

        // They must be at different positions
        assert_ne!(
            binops[0].start, binops[1].start,
            "Duplicate operators must be at distinct positions"
        );

        // Applying each mutation should produce distinct correct outputs
        let mutated0 = apply_mutation(&fm.source, binops[0]);
        let mutated1 = apply_mutation(&fm.source, binops[1]);

        // One mutation: a - b + c, Other: a + b - c
        let has_a_minus = mutated0.contains("a - b + c") || mutated1.contains("a - b + c");
        let has_b_minus = mutated0.contains("a + b - c") || mutated1.contains("a + b - c");
        assert!(
            has_a_minus,
            "One mutant should be 'a - b + c', got: {mutated0} and {mutated1}"
        );
        assert!(
            has_b_minus,
            "One mutant should be 'a + b - c', got: {mutated0} and {mutated1}"
        );
    }

    // INV-2: Applying mutation N produces exactly the expected output (no off-by-one)
    #[test]
    fn test_apply_mutation_exact_positions() {
        // Without spaces: a+b
        let source = "def foo(a, b):\n    return a+b\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let binop = fm
            .mutations
            .iter()
            .find(|m| m.operator == "binop_swap")
            .unwrap();

        // original should be exactly "+"
        assert_eq!(binop.original, "+", "Operator without spaces");
        let mutated = apply_mutation(&fm.source, binop);
        assert!(
            mutated.contains("a-b"),
            "Should produce a-b, got: {mutated}"
        );
        assert!(!mutated.contains("a+b"), "Original + should be gone");
    }

    // INV-3: Nested operators at correct positions
    #[test]
    fn test_nested_operators() {
        let source = "def foo(a, b, c, d):\n    return (a + b) * (c + d)\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();

        // Should have 3 operators: +, *, +
        assert_eq!(binops.len(), 3, "Should find 3 operators: +, *, +");

        // All at different positions
        let positions: std::collections::HashSet<usize> = binops.iter().map(|m| m.start).collect();
        assert_eq!(
            positions.len(),
            3,
            "All operators must be at distinct positions"
        );

        // Each mutation should produce syntactically reasonable output
        for m in &binops {
            let mutated = apply_mutation(&fm.source, m);
            // The mutated source should still contain def and return
            assert!(
                mutated.contains("def foo"),
                "Mutated source should still have def"
            );
            assert!(
                mutated.contains("return"),
                "Mutated source should still have return"
            );
        }
    }

    // Mixed case: x = a + a
    #[test]
    fn test_duplicate_operand_mutation() {
        let source = "def foo(a):\n    x = a + a\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert_eq!(binops.len(), 1, "Should find exactly 1 + operator");

        let mutated = apply_mutation(&fm.source, binops[0]);
        assert!(
            mutated.contains("a - a"),
            "Should produce a - a, got: {mutated}"
        );
    }

    // Byte-span correctness: start and end span exactly the original text
    #[test]
    fn test_mutation_span_correctness() {
        let source = "def foo(a, b, c):\n    return a + b + c\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        for m in &fm.mutations {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original,
                "Span [{}, {}) should equal original '{}'",
                m.start, m.end, m.original
            );
        }
    }
}

#[cfg(test)]
mod match_case_removal_tests {
    use super::*;

    fn match_case_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "match_case_removal")
    }

    // INV-1: A match with 1 case produces 0 mutations.
    #[test]
    fn test_single_case_no_mutations() {
        let source = "def foo(cmd):\n    match cmd:\n        case _:\n            return 0\n";
        let muts = match_case_mutations(source);
        assert!(muts.is_empty(), "1-case match must produce 0 mutations");
    }

    // INV-2: A match with 2 cases produces 2 mutations.
    #[test]
    fn test_two_cases_two_mutations() {
        let source = "def foo(cmd):\n    match cmd:\n        case \"quit\":\n            return 0\n        case _:\n            return 1\n";
        let muts = match_case_mutations(source);
        assert_eq!(muts.len(), 2, "2-case match must produce 2 mutations");
    }

    // INV-3: A match with 3 cases produces 3 mutations.
    #[test]
    fn test_three_cases_three_mutations() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            print(\"hi\")\n",
            "        case _:\n",
            "            print(\"unknown\")\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(muts.len(), 3, "3-case match must produce 3 mutations");
    }

    // INV-4: Generated Python from each mutation is syntactically valid.
    #[test]
    fn test_mutations_produce_valid_python() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            return 1\n",
            "        case _:\n",
            "            return 2\n",
        );
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing case produced invalid Python:\n{mutated}"
            );
        }
    }

    // INV-5: Removing case[0] keeps case[1] and case[2].
    #[test]
    fn test_remove_first_case() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            return 1\n",
            "        case _:\n",
            "            return 2\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let muts: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
            .collect();
        assert_eq!(muts.len(), 3);

        // The mutation that removes case "quit" should keep the other two cases.
        let mutants: Vec<String> = muts.iter().map(|m| apply_mutation(&fm.source, m)).collect();

        // One mutant drops "quit" branch
        assert!(
            mutants.iter().any(|s| !s.contains("\"quit\"")
                && s.contains("\"hello\"")
                && s.contains("case _")),
            "One mutant should remove 'quit' case while keeping 'hello' and '_'"
        );

        // One mutant drops "hello" branch
        assert!(
            mutants.iter().any(|s| s.contains("\"quit\"")
                && !s.contains("\"hello\"")
                && s.contains("case _")),
            "One mutant should remove 'hello' case while keeping 'quit' and '_'"
        );

        // One mutant drops wildcard branch
        assert!(
            mutants.iter().any(|s| s.contains("\"quit\"")
                && s.contains("\"hello\"")
                && !s.contains("case _")),
            "One mutant should remove '_' case while keeping 'quit' and 'hello'"
        );
    }

    // INV-6: Nested match statements each produce their own mutations independently.
    #[test]
    fn test_nested_match_independent_mutations() {
        let source = concat!(
            "def foo(cmd, sub):\n",
            "    match cmd:\n",
            "        case \"outer_a\":\n",
            "            match sub:\n",
            "                case \"inner_1\":\n",
            "                    return 0\n",
            "                case \"inner_2\":\n",
            "                    return 1\n",
            "        case \"outer_b\":\n",
            "            return 2\n",
        );
        let muts = match_case_mutations(source);
        // Outer match has 2 cases → 2 mutations.
        // Inner match has 2 cases → 2 mutations.
        // Total: 4 match_case_removal mutations.
        assert_eq!(
            muts.len(),
            4,
            "Outer (2 cases) + inner (2 cases) = 4 match_case_removal mutations, got: {muts:?}"
        );
    }

    // INV-7: Span correctness — original text equals func_source[start..end].
    #[test]
    fn test_span_correctness() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original,
                "Span [{}, {}) must equal original",
                m.start, m.end
            );
        }
    }

    // INV-8: Indentation is preserved in remaining cases after removal.
    #[test]
    fn test_indentation_preserved() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let muts: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
            .collect();
        assert_eq!(muts.len(), 2);

        for m in &muts {
            let mutated = apply_mutation(&fm.source, m);
            // The remaining "case" line must still be indented by 8 spaces.
            assert!(
                mutated.contains("        case "),
                "Case indentation must be preserved: {mutated:?}"
            );
        }
    }

    // --- lambda_mutation splice correctness tests ---

    fn lambda_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "lambda_mutation")
    }

    // INV-1: `lambda x: x if x else None` — body text contains `x` which also appears in
    // params; the mutation must replace only the body, not the parameter.
    #[test]
    fn test_lambda_mutation_body_text_in_params() {
        let source = "def foo():\n    f = lambda x: x\n";
        let muts = lambda_mutations(source);
        assert!(!muts.is_empty(), "should find a lambda mutation");
        let m = &muts[0];
        // The replacement must not corrupt the parameter list.
        assert!(
            m.replacement.contains("lambda x: None"),
            "param `x` must be untouched; replacement was: {}",
            m.replacement
        );
        // The old String::replace() bug would have produced "lambda None: None".
        assert!(
            !m.replacement.contains("lambda None"),
            "param must not be replaced; replacement was: {}",
            m.replacement
        );
    }

    // INV-1 (extended): complex body that includes the param name multiple times
    #[test]
    fn test_lambda_mutation_complex_body_with_param_name() {
        let source = "def foo():\n    f = lambda x: x if x else None\n";
        let fms = collect_file_mutations(source);
        let muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "lambda_mutation")
            .collect();
        assert!(!muts.is_empty(), "should find a lambda mutation");
        let m = &muts[0];
        // Body is `x if x else None` → replacement body is `None` (since body != "None").
        // The param `x` must remain untouched.
        assert!(
            m.replacement.starts_with("lambda x:"),
            "param `x` must be preserved; replacement was: {}",
            m.replacement
        );
        assert!(
            m.replacement.ends_with("None"),
            "body should be replaced with None; replacement was: {}",
            m.replacement
        );
    }

    // INV-2: `lambda: 0` (no params) — body `0` → `None` via lambda mutation
    #[test]
    fn test_lambda_mutation_no_params() {
        let source = "def foo():\n    f = lambda: 0\n";
        let muts = lambda_mutations(source);
        // Lambda body `0` is a number — lambda_mutation replaces it with `None`.
        let lam_mut = muts.iter().find(|m| m.replacement.contains("lambda: None"));
        assert!(
            lam_mut.is_some(),
            "lambda: 0 should produce lambda: None; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-3: applying any lambda mutation via apply_mutation() must produce parseable Python
    #[test]
    fn test_lambda_mutation_produces_parseable_python() {
        let cases = [
            "def foo():\n    f = lambda x: x\n",
            "def foo():\n    f = lambda x: x if x else None\n",
            "def foo():\n    f = lambda: 0\n",
            "def foo():\n    f = lambda a, b: a + b\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "lambda_mutation")
                {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "lambda mutation produced unparseable Python for input {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-4: `lambda: None` — body `None` → `0` (reverse direction)
    #[test]
    fn test_lambda_mutation_body_none_replaced_with_zero() {
        let source = "def foo():\n    f = lambda: None\n";
        let muts = lambda_mutations(source);
        let lam_mut = muts.iter().find(|m| m.replacement.contains("lambda: 0"));
        assert!(
            lam_mut.is_some(),
            "lambda: None should produce lambda: 0; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-9: String literal containing `match x:` in a preceding statement does not
    // confuse the match-header search — only the real match generates case removals.
    #[test]
    fn test_preceding_string_with_match_pattern() {
        let source = concat!(
            "def foo(x):\n",
            "    s = \"match x:\"\n", // string literal looks like a match header
            "    match x:\n",
            "        case 1:\n",
            "            return 1\n",
            "        case 2:\n",
            "            return 2\n",
        );
        let muts = match_case_mutations(source);
        // Only the real match (2 cases) should produce mutations.
        assert_eq!(
            muts.len(),
            2,
            "Preceding string with 'match x:' must not generate extra mutations; got: {muts:?}"
        );
    }

    // INV-10: Case body containing `case _:` in a comment does not produce a false match
    // when searching for the next case — the real second case is still correctly found.
    #[test]
    fn test_case_keyword_in_comment_not_matched() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"a\":\n",
            "            # TODO: case _: should also handle fallback\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(
            muts.len(),
            2,
            "Comment containing 'case _:' must not produce a false match; got: {muts:?}"
        );
        // Each mutation must produce valid Python.
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing case produced invalid Python:\n{mutated}"
            );
        }
    }

    // INV-11: Match with guarded cases (case x if cond:) correctly locates case starts.
    #[test]
    fn test_guarded_cases() {
        let source = concat!(
            "def foo(x):\n",
            "    match x:\n",
            "        case 1 if x > 0:\n",
            "            return 1\n",
            "        case 2 if x > 0:\n",
            "            return 2\n",
            "        case _:\n",
            "            return 3\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(
            muts.len(),
            3,
            "Guarded cases must each generate a removal mutation; got: {muts:?}"
        );
        // All mutants must parse.
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing guarded case produced invalid Python:\n{mutated}"
            );
        }
    }
}

#[cfg(test)]
mod assignment_mutation_tests {
    use super::*;

    fn assignment_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "assignment_mutation")
    }

    fn assignment_mutants(source: &str) -> Vec<String> {
        let fms = collect_file_mutations(source);
        let fm = fms.into_iter().next().expect("should have mutations");
        fm.mutations
            .iter()
            .filter(|m| m.operator == "assignment_mutation")
            .map(|m| apply_mutation(&fm.source, m))
            .collect()
    }

    // INV-1: `x = y == z` — value contains `==`; first `=` in text is still the assignment `=`.
    // The mutation must produce `x = None`, not a truncated result from matching `=` inside `==`.
    #[test]
    fn test_assignment_value_with_comparison() {
        let source = "def foo(y, z):\n    x = y == z\n";
        let muts = assignment_mutations(source);
        assert_eq!(muts.len(), 1, "should find one assignment mutation");
        // m.replacement is the full replaced span text
        assert_eq!(
            muts[0].replacement, "x = None",
            "replacement should be 'x = None'; got {:?}",
            muts[0].replacement
        );
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1);
        assert!(
            mutants[0].contains("x = None"),
            "mutated source should contain 'x = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-2: `x = d['=']` — value contains a string literal with `=`; must not confuse the splitter.
    #[test]
    fn test_assignment_value_with_eq_in_string() {
        let source = "def foo(d):\n    x = d['=']\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1);
        assert!(
            mutants[0].contains("x = None"),
            "mutated source should contain 'x = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-3: `a = b = c` — chained assignment has two targets.
    // The mutation must replace the value `c` with `None`, preserving both targets: `a = b = None`.
    // The old find('=') approach would produce `a = None`, silently dropping `b` as a target.
    #[test]
    fn test_chained_assignment_preserves_all_targets() {
        let source = "def foo(c):\n    a = b = c\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1, "chained assignment should produce exactly one assignment mutation");
        assert!(
            mutants[0].contains("a = b = None"),
            "chained assignment must produce 'a = b = None' (both targets preserved); got:\n{}",
            mutants[0]
        );
    }

    // INV-4: `a, b = 1, 2` — tuple unpacking (single AssignTarget with a Tuple target).
    // The mutation must produce `a, b = None`.
    #[test]
    fn test_tuple_unpacking_assignment() {
        let source = "def foo():\n    a, b = 1, 2\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1, "tuple unpacking should produce one assignment mutation");
        assert!(
            mutants[0].contains("a, b = None"),
            "tuple unpacking assignment must produce 'a, b = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-5: All assignment mutations produce syntactically valid Python.
    #[test]
    fn test_all_assignment_mutations_produce_valid_python() {
        let sources = [
            "def foo(y, z):\n    x = y == z\n",
            "def foo(d):\n    x = d['=']\n",
            "def foo(c):\n    a = b = c\n",
            "def foo():\n    a, b = 1, 2\n",
            "def foo():\n    x = 1\n",
            "def foo():\n    x = None\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            if let Some(fm) = fms.first() {
                for m in fm.mutations.iter().filter(|m| m.operator == "assignment_mutation") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "assignment_mutation on {:?} produced unparseable Python:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-6: `x = None` — when the current value is already None, mutate to `""`.
    #[test]
    fn test_assignment_none_to_empty_string() {
        let source = "def foo():\n    x = None\n";
        let muts = assignment_mutations(source);
        assert_eq!(muts.len(), 1);
        // m.replacement is the full replaced span text
        assert_eq!(
            muts[0].replacement, "x = \"\"",
            "when value is None, full replacement must be 'x = \"\"'; got {:?}",
            muts[0].replacement
        );
        let mutants = assignment_mutants(source);
        assert!(
            mutants[0].contains("x = \"\""),
            "must produce 'x = \"\"'; got:\n{}",
            mutants[0]
        );
    }
}

// --- Unary operation mutation tests ---
#[cfg(test)]
mod unary_mutation_tests {
    use super::*;

    fn unary_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "unary_removal")
    }

    // INV-1: `not x` → `x` removes the unary `not` operator.
    #[test]
    fn test_not_removal() {
        let source = "def foo(x):\n    return not x\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "should find unary_removal mutation for `not x`");
        let m = &muts[0];
        assert_eq!(m.original, "not x", "original should be the full `not x` expression");
        assert_eq!(m.replacement, "x", "replacement should be just `x`");
    }

    // INV-2: `~x` → `x` removes the bitwise invert operator.
    #[test]
    fn test_bit_invert_removal() {
        let source = "def foo(x):\n    return ~x\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "should find unary_removal mutation for `~x`");
        let m = &muts[0];
        assert_eq!(m.replacement, "x", "replacement should be just `x`");
    }

    // INV-3: Unary `-` is NOT removed (only `not` and `~` are removed).
    #[test]
    fn test_minus_not_removed() {
        let source = "def foo(x):\n    return -x\n";
        let muts = unary_mutations(source);
        assert!(muts.is_empty(), "unary minus must not produce unary_removal mutation");
    }

    // INV-4: Correct byte span — source[start..end] == original.
    #[test]
    fn test_unary_span_correctness() {
        let source = "def foo(x):\n    return not x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm.mutations.iter().filter(|m| m.operator == "unary_removal") {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "INV-4: span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }

    // INV-5: All unary mutations produce parseable Python.
    #[test]
    fn test_unary_mutation_produces_parseable_python() {
        let cases = [
            "def foo(x):\n    return not x\n",
            "def foo(x):\n    return ~x\n",
            "def foo(a, x):\n    return not x and a > 0\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "unary_removal") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "unary_removal mutation produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-6: `not x` mutation is found even when inside a compound expression.
    #[test]
    fn test_not_inside_and_expression() {
        let source = "def foo(a, b):\n    return not a and b > 0\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "unary_removal should be found inside compound expression");
    }
}

// --- Unary swap mutation tests ---
#[cfg(test)]
mod unary_swap_tests {
    use super::*;

    fn unary_swap_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "unary_swap")
    }

    // INV-1: `-x` → `+x`
    #[test]
    fn test_minus_swapped_to_plus() {
        let source = "def foo(x):\n    return -x\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `-x`");
        let m = &muts[0];
        assert_eq!(m.original, "-x");
        assert_eq!(m.replacement, "+x");
    }

    // INV-2: `+x` → `-x`
    #[test]
    fn test_plus_swapped_to_minus() {
        let source = "def foo(x):\n    return +x\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `+x`");
        let m = &muts[0];
        assert_eq!(m.original, "+x");
        assert_eq!(m.replacement, "-x");
    }

    // INV-3: `-5` → `+5` (literal numbers)
    #[test]
    fn test_minus_literal_swapped_to_plus() {
        let source = "def foo():\n    return -5\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `-5`");
        assert_eq!(muts[0].replacement, "+5");
    }

    // INV-4: `-x` produces both unary_swap (+x) and existing unary_removal (x)
    #[test]
    fn test_minus_produces_both_swap_and_removal_not_produced() {
        // unary_removal only applies to `not` and `~`, not `-`
        let source = "def foo(x):\n    return -x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.into_iter().flat_map(|fm| fm.mutations.into_iter()).collect();
        let swaps: Vec<_> = all_muts.iter().filter(|m| m.operator == "unary_swap").collect();
        let removals: Vec<_> = all_muts.iter().filter(|m| m.operator == "unary_removal").collect();
        assert!(!swaps.is_empty(), "should have unary_swap for `-x`");
        assert!(removals.is_empty(), "unary_removal must NOT fire for `-x`");
    }

    // INV-5: `not` and `~` do NOT get unary_swap
    #[test]
    fn test_not_and_bitnot_do_not_get_swap() {
        let source_not = "def foo(x):\n    return not x\n";
        let source_inv = "def foo(x):\n    return ~x\n";
        assert!(unary_swap_mutations(source_not).is_empty(), "`not x` must not get unary_swap");
        assert!(unary_swap_mutations(source_inv).is_empty(), "`~x` must not get unary_swap");
    }

    // INV-6: All unary_swap mutations produce parseable Python.
    #[test]
    fn test_unary_swap_produces_parseable_python() {
        let cases = [
            "def foo(x):\n    return -x\n",
            "def foo(x):\n    return +x\n",
            "def foo():\n    return -5\n",
            "def foo(x, y):\n    return -(x + y)\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "unary_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "unary_swap produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- String emptying mutation tests ---
#[cfg(test)]
mod string_emptying_tests {
    use super::*;

    fn string_emptying_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "string_emptying")
    }

    // INV-1: Non-empty string gets both string_mutation (XX) and string_emptying ("") mutations.
    #[test]
    fn test_nonempty_string_gets_both_mutations() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.into_iter().flat_map(|fm| fm.mutations.into_iter()).collect();
        let xx_muts: Vec<_> = all_muts.iter().filter(|m| m.operator == "string_mutation").collect();
        let empty_muts: Vec<_> = all_muts.iter().filter(|m| m.operator == "string_emptying").collect();
        assert!(!xx_muts.is_empty(), "should find string_mutation (XX) for non-empty string");
        assert!(!empty_muts.is_empty(), "should find string_emptying for non-empty string");
        assert_eq!(empty_muts[0].replacement, "\"\"", "emptying replacement should be empty string");
    }

    // INV-2: Already-empty string does NOT get string_emptying (skip if already empty).
    #[test]
    fn test_already_empty_string_not_emptied() {
        let source = "def foo():\n    return \"\"\n";
        let muts = string_emptying_mutations(source);
        assert!(muts.is_empty(), "empty string should not get string_emptying mutation");
    }

    // INV-3: Quote character is preserved in emptied string.
    #[test]
    fn test_empty_uses_same_quote_char() {
        let source = "def foo():\n    return 'hello'\n";
        let muts = string_emptying_mutations(source);
        assert!(!muts.is_empty(), "single-quoted string should get string_emptying");
        assert_eq!(muts[0].replacement, "''", "should use single quotes for emptied string");
    }

    // INV-4: Triple-quoted strings (docstrings) do NOT get string_emptying.
    #[test]
    fn test_triple_quoted_strings_not_emptied() {
        let source = "def foo():\n    \"\"\"This is a docstring.\"\"\"\n    return 1\n";
        let muts = string_emptying_mutations(source);
        assert!(muts.is_empty(), "docstrings must not get string_emptying");
    }

    // INV-5: All string_emptying mutations produce parseable Python.
    #[test]
    fn test_string_emptying_produces_parseable_python() {
        let cases = [
            "def greet():\n    return \"hello\"\n",
            "def foo():\n    return 'world'\n",
            "def bar(x):\n    return x.replace('a', 'b')\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "string_emptying") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "string_emptying produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- Float mutation tests ---
#[cfg(test)]
mod float_mutation_tests {
    use super::*;

    fn float_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "number_mutation")
    }

    // INV-1: `1.5` → `2.5` (float + 1.0).
    #[test]
    fn test_float_incremented_by_one() {
        let source = "def foo():\n    return 1.5\n";
        let muts = float_mutations(source);
        assert!(!muts.is_empty(), "should find number_mutation for float 1.5");
        let m = &muts[0];
        assert_eq!(m.replacement, "2.5", "1.5 should become 2.5");
    }

    // INV-2: `0.0` → `1.0`.
    #[test]
    fn test_float_zero_incremented() {
        let source = "def foo():\n    return 0.0\n";
        let muts = float_mutations(source);
        assert!(!muts.is_empty(), "should find number_mutation for float 0.0");
        let m = &muts[0];
        assert_eq!(m.replacement, "1", "0.0 should become 1 (1.0 after formatting)");
    }

    // INV-3: Float mutation produces parseable Python.
    #[test]
    fn test_float_mutation_parseable_python() {
        let cases = ["def foo():\n    return 1.5\n", "def foo():\n    return 0.0\n"];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "number_mutation") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "float mutation produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-4: Correct byte span for float.
    #[test]
    fn test_float_span_correctness() {
        let source = "def foo():\n    return 1.5\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm.mutations.iter().filter(|m| m.operator == "number_mutation") {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }
}

// --- AugAssign mutation tests ---
#[cfg(test)]
mod augassign_mutation_tests {
    use super::*;

    fn augop_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "augop_swap")
    }

    fn augassign_to_assign_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "augassign_to_assign")
    }

    // INV-1: `a += b` → `a -= b` (augop_swap).
    #[test]
    fn test_add_assign_swapped_to_sub_assign() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty(), "should find augop_swap for +=");
        assert!(muts.iter().any(|m| m.replacement.contains("-=")), "should swap += to -=");
    }

    // INV-2: `a -= b` → `a += b`.
    #[test]
    fn test_sub_assign_swapped_to_add_assign() {
        let source = "def foo(a, b):\n    a -= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("+=")), "should swap -= to +=");
    }

    // INV-3: `a *= b` → `a /= b`.
    #[test]
    fn test_mul_assign_swapped_to_div_assign() {
        let source = "def foo(a, b):\n    a *= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("/=")), "should swap *= to /=");
    }

    // INV-4: `a //= b` → `a /= b`.
    #[test]
    fn test_floordiv_assign_swapped() {
        let source = "def foo(a, b):\n    a //= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        // //= → /= (trimmed comparison)
        assert!(muts.iter().any(|m| m.replacement.trim() == "/="), "should swap //= to /=");
    }

    // INV-5: `a **= b` → `a *= b`.
    #[test]
    fn test_pow_assign_swapped_to_mul_assign() {
        let source = "def foo(a, b):\n    a **= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("*=")), "should swap **= to *=");
    }

    // INV-6: `a <<= b` → `a >>= b`.
    #[test]
    fn test_lshift_assign_swapped_to_rshift_assign() {
        let source = "def foo(a, b):\n    a <<= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains(">>=")), "should swap <<= to >>=");
    }

    // INV-7: `a >>= b` → `a <<= b`.
    #[test]
    fn test_rshift_assign_swapped_to_lshift_assign() {
        let source = "def foo(a, b):\n    a >>= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("<<=")), "should swap >>= to <<=");
    }

    // INV-8: `a &= b` → `a |= b`.
    #[test]
    fn test_and_assign_swapped_to_or_assign() {
        let source = "def foo(a, b):\n    a &= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("|=")), "should swap &= to |=");
    }

    // INV-9: `a |= b` → `a &= b`.
    #[test]
    fn test_or_assign_swapped_to_and_assign() {
        let source = "def foo(a, b):\n    a |= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("&=")), "should swap |= to &=");
    }

    // INV-10: `a ^= b` → `a &= b`.
    #[test]
    fn test_xor_assign_swapped_to_and_assign() {
        let source = "def foo(a, b):\n    a ^= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("&=")), "should swap ^= to &=");
    }

    // INV-11: `a += b` → `a = b` (augassign_to_assign).
    #[test]
    fn test_augassign_to_assign_conversion() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let muts = augassign_to_assign_mutations(source);
        assert!(!muts.is_empty(), "should find augassign_to_assign mutation");
        // The replacement should be `a = b` (the plain assignment form).
        assert!(
            muts.iter().any(|m| m.replacement.contains("a =") && !m.replacement.contains("+=")),
            "augassign_to_assign should produce plain `a = b`; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-12: All augop mutations produce parseable Python.
    #[test]
    fn test_augop_mutations_parseable() {
        let cases = [
            "def foo(a, b):\n    a += b\n    return a\n",
            "def foo(a, b):\n    a -= b\n    return a\n",
            "def foo(a, b):\n    a *= b\n    return a\n",
            "def foo(a, b):\n    a //= b\n    return a\n",
            "def foo(a, b):\n    a **= b\n    return a\n",
            "def foo(a, b):\n    a <<= b\n    return a\n",
            "def foo(a, b):\n    a >>= b\n    return a\n",
            "def foo(a, b):\n    a &= b\n    return a\n",
            "def foo(a, b):\n    a |= b\n    return a\n",
            "def foo(a, b):\n    a ^= b\n    return a\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "augop_swap" || m.operator == "augassign_to_assign")
                {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "augop mutation {:?} produced unparseable Python for {:?}:\n{}",
                        m.operator,
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-13: Correct byte span for augop — source[start..end] == original.
    #[test]
    fn test_augop_span_correctness() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations");
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "augop_swap" || m.operator == "augassign_to_assign")
        {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }
}

// --- IfExp (ternary) mutation tests ---
#[cfg(test)]
mod ifexp_mutation_tests {
    use super::*;

    // INV-1: `x + 1 if True else y - 1` — mutations found for both `+` and `-` inside ternary.
    #[test]
    fn test_ifexp_recurses_into_body_and_orelse() {
        let source = "def foo(x, y):\n    return x + 1 if True else y - 1\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should collect mutations from ifexp");
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        // Should find both the `+` in `x + 1` and the `-` in `y - 1`
        assert!(binops.len() >= 2, "should find binop mutations inside ternary body and orelse");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_sub = binops.iter().any(|m| m.original.trim() == "-");
        assert!(has_add, "should find + → - mutation in ternary body");
        assert!(has_sub, "should find - → + mutation in ternary orelse");
    }

    // INV-2: Mutations inside ternary produce parseable Python.
    #[test]
    fn test_ifexp_mutations_parseable() {
        let source = "def foo(x, y):\n    return x + 1 if True else y - 1\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "ifexp mutation {:?} produced unparseable Python:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }
}

// --- Container literal recursion tests ---
#[cfg(test)]
mod container_mutation_tests {
    use super::*;

    // INV-1: Tuple — mutations found inside tuple elements.
    #[test]
    fn test_tuple_elements_mutated() {
        let source = "def foo(a, b, c, d):\n    return (a + b, c * d)\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        // should find `+` and `*`
        assert!(binops.len() >= 2, "should find binop mutations inside tuple elements");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_mul = binops.iter().any(|m| m.original.trim() == "*");
        assert!(has_add, "should mutate `+` inside tuple");
        assert!(has_mul, "should mutate `*` inside tuple");
    }

    // INV-2: Empty tuple must not crash.
    #[test]
    fn test_empty_tuple_no_crash() {
        let source = "def foo():\n    return ()\n";
        let fms = collect_file_mutations(source);
        // No binop mutations; function may be excluded (no mutable ops). Just must not crash.
        let _ = fms;
    }

    // INV-3: List — mutations found inside list elements.
    #[test]
    fn test_list_elements_mutated() {
        let source = "def foo(a, b, c, d):\n    return [a + b, c - d]\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(binops.len() >= 2, "should find binop mutations inside list elements");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_sub = binops.iter().any(|m| m.original.trim() == "-");
        assert!(has_add, "should mutate `+` inside list");
        assert!(has_sub, "should mutate `-` inside list");
    }

    // INV-4: Empty list must not crash.
    #[test]
    fn test_empty_list_no_crash() {
        let source = "def foo():\n    return []\n";
        let fms = collect_file_mutations(source);
        let _ = fms;
    }

    // INV-5: Dict — mutations found in dict values.
    #[test]
    fn test_dict_value_mutated() {
        let source = "def foo(a, b):\n    return {'key': a + b}\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!binops.is_empty(), "should find binop mutation inside dict value");
    }

    // INV-6: Empty dict must not crash.
    #[test]
    fn test_empty_dict_no_crash() {
        let source = "def foo():\n    return {}\n";
        let fms = collect_file_mutations(source);
        let _ = fms;
    }

    // INV-7: Subscript — mutations found in sub.value (the subscripted object).
    // The subscript arm recurses into sub.value, so mutations on the object are found.
    // Note: the slice expression is NOT recursed into by the current implementation.
    #[test]
    fn test_subscript_value_mutated() {
        // d.lower()[0] — subscript arm recurses into sub.value = d.lower() (a Call),
        // which produces a method_swap mutation for .lower() → .upper().
        let source = "def foo(d):\n    return d.lower()[0]\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let method_muts: Vec<_> =
            fm.mutations.iter().filter(|m| m.operator == "method_swap").collect();
        assert!(
            !method_muts.is_empty(),
            "subscript arm should recurse into sub.value and find method_swap mutation"
        );
    }

    // INV-8: All container literal mutations produce parseable Python.
    #[test]
    fn test_container_mutations_parseable() {
        let cases = [
            "def foo(a, b, c, d):\n    return (a + b, c * d)\n",
            "def foo(a, b, c, d):\n    return [a + b, c - d]\n",
            "def foo(a, b):\n    return {'key': a + b}\n",
            "def foo(d):\n    return d.lower()[0]\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "container mutation {:?} produced unparseable Python for {:?}:\n{}",
                        m.operator,
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- Assert statement mutation tests ---
#[cfg(test)]
mod assert_mutation_tests {
    use super::*;

    // INV-1: `assert x + 1` — `+` inside assert test should be mutated.
    #[test]
    fn test_assert_test_expression_mutated() {
        let source = "def foo(x):\n    assert x + 1\n    return x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!binops.is_empty(), "binop inside assert test should be mutated");
    }

    // INV-2: Assert mutation produces parseable Python.
    #[test]
    fn test_assert_mutation_parseable() {
        let source = "def foo(x):\n    assert x + 1\n    return x\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "assert mutation {:?} produced unparseable Python:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }

    // INV-3: `assert a > b` — comparison inside assert is also mutated.
    #[test]
    fn test_assert_comparison_mutated() {
        let source = "def foo(a, b):\n    assert a > b\n    return a\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let compops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "compop_swap").collect();
        assert!(!compops.is_empty(), "comparison inside assert test should be mutated");
    }
}

// --- Yield detection tests ---
#[cfg(test)]
mod yield_detection_tests {
    use super::*;

    // Helper: collect mutations from the first function and return its is_generator flag.
    // Returns Err if the source contains no function definitions.
    fn check_yield_in_source(source: &str) -> anyhow::Result<bool> {
        let fms = collect_file_mutations(source);
        fms.first()
            .map(|fm| fm.is_generator)
            .ok_or_else(|| anyhow::anyhow!("no function def found in source"))
    }

    // INV-1: `yield` inside an `if` block → detected.
    #[test]
    fn test_yield_inside_if_detected() {
        // Note: we need a mutable expr to ensure collect_file_mutations works if called,
        // but here we directly test suite_contains_yield.
        let source = "def gen():\n    if True:\n        yield 1\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside if must be detected");
    }

    // INV-2: `yield` inside a `while` loop → detected.
    #[test]
    fn test_yield_inside_while_detected() {
        let source = "def gen():\n    while True:\n        yield 1\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside while must be detected");
    }

    // INV-3: `yield` inside a `for` loop → detected.
    #[test]
    fn test_yield_inside_for_detected() {
        let source = "def gen(items):\n    for x in items:\n        yield x\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside for must be detected");
    }

    // INV-4: `yield` inside a `with` block → detected.
    #[test]
    fn test_yield_inside_with_detected() {
        let source = "def gen(f):\n    with open(f) as h:\n        yield h.read()\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside with must be detected");
    }

    // INV-5: `yield` inside `try/except` → detected.
    #[test]
    fn test_yield_inside_try_detected() {
        let source = "def gen():\n    try:\n        yield 1\n    except Exception:\n        pass\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside try must be detected");
    }

    // INV-6: `yield` inside a nested `def` → NOT detected (must not recurse past FunctionDef).
    #[test]
    fn test_yield_inside_nested_def_not_detected() {
        let source = "def outer():\n    def inner():\n        yield 1\n    return 0\n";
        assert!(
            !check_yield_in_source(source).unwrap(),
            "yield inside nested def must NOT make outer a generator"
        );
    }

    // INV-7: No yield anywhere → not detected.
    #[test]
    fn test_no_yield_not_detected() {
        let source = "def foo():\n    return 1 + 2\n";
        assert!(!check_yield_in_source(source).unwrap(), "function without yield must not be detected");
    }

    // INV-8: `yield from` → detected.
    #[test]
    fn test_yield_from_detected() {
        let source = "def gen(items):\n    yield from items\n";
        assert!(check_yield_in_source(source).unwrap(), "yield from must be detected");
    }

    // INV-9: Top-level `yield` (simple return body style) → detected.
    #[test]
    fn test_top_level_yield_detected() {
        let source = "def gen():\n    yield 1\n";
        assert!(check_yield_in_source(source).unwrap(), "top-level yield must be detected");
    }

    // INV-10: `yield` inside `except` handler (not in body) → detected.
    #[test]
    fn test_yield_inside_except_handler_detected() {
        let source = "def gen():\n    try:\n        pass\n    except Exception:\n        yield 0\n";
        assert!(check_yield_in_source(source).unwrap(), "yield inside except handler must be detected");
    }

    // INV-0: source with no function definitions returns Err instead of panicking.
    #[test]
    fn test_no_function_def_returns_err() {
        assert!(check_yield_in_source("").is_err(), "empty source must return Err, not panic");
        assert!(check_yield_in_source("x = 1\n").is_err(), "source with no function def must return Err");
    }

    // INV-11: `yield` only inside nested def — outer is_generator flag is correctly False.
    // Exercises the is_generator field of FunctionMutations by collecting mutations.
    #[test]
    fn test_outer_is_generator_false_when_yield_only_in_nested_def() {
        // outer needs a mutation so it gets collected; use a comparison.
        let source = "def outer(n):\n    if n > 0:\n        def inner():\n            yield n\n    return n\n";
        let fms = collect_file_mutations(source);
        let outer = fms.iter().find(|fm| fm.name == "outer").expect("outer should be collected");
        assert!(
            !outer.is_generator,
            "outer must not be is_generator just because nested def has yield"
        );
    }

    // --- default_arg tests ---

    #[test]
    fn test_default_int_incremented() {
        let source = "def f(x=0):\n    return x\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "0");
        assert_eq!(m.replacement, "1");
        // Offset correctness: the `0` default is at position 8 in "def f(x=0):\n    return x\n"
        assert_eq!(&fms[0].source[m.start..m.end], "0", "source slice must equal original");
    }

    #[test]
    fn test_default_none_to_empty_string() {
        let source = "def f(x=None):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "None");
        assert_eq!(m.replacement, "\"\"");
        assert_eq!(&fms[0].source[m.start..m.end], "None");
    }

    #[test]
    fn test_default_string_to_xx() {
        let source = "def f(x=\"hello\"):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "\"hello\"");
        assert_eq!(m.replacement, "\"XXhelloXX\"");
        assert_eq!(&fms[0].source[m.start..m.end], "\"hello\"");
    }

    #[test]
    fn test_default_bool_swapped() {
        let source = "def f(x=True):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "True");
        assert_eq!(m.replacement, "False");
        assert_eq!(&fms[0].source[m.start..m.end], "True");
    }

    #[test]
    fn test_no_default_no_mutation() {
        let source = "def f(x):\n    return x + 1\n";
        let fms = collect_file_mutations(source);
        let default_muts: Vec<_> = fms[0].mutations.iter()
            .filter(|m| m.operator == "default_arg")
            .collect();
        assert!(default_muts.is_empty(), "param without default should produce no default_arg mutation");
    }

    #[test]
    fn test_multiple_defaults_independent() {
        let source = "def f(x=0, y=1):\n    return x + y\n";
        let fms = collect_file_mutations(source);
        let default_muts: Vec<_> = fms[0].mutations.iter()
            .filter(|m| m.operator == "default_arg")
            .collect();
        assert_eq!(default_muts.len(), 2, "two params with defaults → two mutations");
        // x=0 → x=1
        let mx = default_muts.iter().find(|m| m.original == "0").expect("mutation for x=0");
        assert_eq!(mx.replacement, "1");
        assert_eq!(&fms[0].source[mx.start..mx.end], "0");
        // y=1 → y=2
        let my = default_muts.iter().find(|m| m.original == "1").expect("mutation for y=1");
        assert_eq!(my.replacement, "2");
        assert_eq!(&fms[0].source[my.start..my.end], "1");
    }

    #[test]
    fn test_default_arg_span_correctness() {
        // Verify that applying each default_arg mutation to func_source produces valid output.
        let sources = [
            "def f(x=0):\n    return x\n",
            "def f(x=None):\n    return x\n",
            "def f(x=\"hello\"):\n    return x\n",
            "def f(x=True):\n    return x\n",
            "def f(x=0, y=1):\n    return x + y\n",
        ];
        for source in sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "default_arg") {
                    // Span correctness: source[start..end] == original
                    assert_eq!(
                        &fm.source[m.start..m.end], m.original.as_str(),
                        "span mismatch for source: {source}"
                    );
                    // Replacement differs
                    assert_ne!(m.original, m.replacement, "replacement must differ");
                }
            }
        }
    }

    #[test]
    fn test_default_arg_parseable() {
        // After applying each default_arg mutation, the resulting function must parse as valid Python.
        let sources = [
            "def f(x=0):\n    return x\n",
            "def f(x=None):\n    return x\n",
            "def f(x=\"hello\"):\n    return x\n",
            "def f(x=True):\n    return x\n",
            "def f(x=False):\n    return x\n",
            "def f(x=3.14):\n    return x\n",
            "def f(x=0, y=1):\n    return x\n",
        ];
        for source in sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "default_arg") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "mutated source must parse as valid Python:\n{mutated}\n(original: {source})"
                    );
                }
            }
        }
    }

    // Kills mutant: line 329 `||` → `&&` (float detection via `.`) and
    //               line 332 `!=` → `==` (float dedup guard).
    // A simple float like `1.5` contains `.` but NOT `e`, so with `&&` it would
    // skip the float branch entirely — no default_arg mutation would be emitted.
    // With `==`, the dedup guard `r != trimmed` would flip to `r == trimmed`, which
    // is never true for n+1.0 vs n, so the mutation would be suppressed.
    #[test]
    fn test_default_float_simple() {
        let source = "def f(x=1.5):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x=1.5) must produce a default_arg mutation");
        assert_eq!(m.original, "1.5");
        assert_eq!(m.replacement, "2.5");
        assert_eq!(&fms[0].source[m.start..m.end], "1.5");
    }

    // Kills mutant: line 329 `||` → `&&` via the `e` branch.
    // `1e2` contains `e` but NOT `.`, so with `&&` the float branch would be skipped.
    #[test]
    fn test_default_float_scientific() {
        let source = "def f(x=1e2):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x=1e2) must produce a default_arg mutation");
        assert_eq!(m.original, "1e2");
        // 1e2 = 100.0, +1.0 = 101.0
        assert_eq!(m.replacement, "101");
        assert_eq!(&fms[0].source[m.start..m.end], "1e2");
    }

    // Kills mutant: line 343 `==` → `!=` (triple-quote detection: `quote_char == '"'`).
    // Flipping to `!=` would choose `'''` as the triple for a `"`-quoted string, so
    // `!rest.starts_with("'''")` would be true for `'hello'` but the wrong check runs.
    // More directly: single-quoted string `'hello'` must produce a mutation.
    #[test]
    fn test_default_single_quoted_string() {
        let source = "def f(x='hello'):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x='hello') must produce a default_arg mutation");
        assert_eq!(m.original, "'hello'");
        assert_eq!(m.replacement, "'XXhelloXX'");
        assert_eq!(&fms[0].source[m.start..m.end], "'hello'");
    }

    // Kills mutant: line 344 `&&` → `||` (compound guard weakening).
    // Triple-quoted `"""doc"""` falls through to the `None` fallback — replacement must be "None".
    // If either `&&` becomes `||`, the guard weakens: `!starts_with(triple) || ends_with(q)` is
    // true for `"""doc"""` (ends_with `"` is true), so it would enter the string branch and
    // produce `"""XXdocXX"""` instead. The test pins the replacement to "None".
    #[test]
    fn test_default_triple_quoted_fallback() {
        let source = "def f(x=\"\"\"doc\"\"\"):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("triple-quoted default must still produce a default_arg mutation via fallback");
        // Must fall back to None replacement, NOT wrap with XX (which would happen if && → ||)
        assert_eq!(
            m.replacement, "None",
            "triple-quoted string must get fallback 'None' replacement, not XX-wrapping"
        );
        assert_ne!(
            m.replacement, "\"\"\"XXdocXX\"\"\"",
            "triple-quoted string must not be XX-wrapped"
        );
    }

    // Kills mutant: line 344 second `&&` → `||` (ends_with guard).
    // Confirms both sides of the compound guard work independently.
    // - single-quoted `'hi'`: must produce XX-wrapped mutation (not None fallback)
    // - triple-quoted `'''hi'''`: must produce None fallback (not XX-wrapped)
    #[test]
    fn test_default_string_guard_compound() {
        // Normal single-quoted: must produce XX mutation
        let source_single = "def f(x='hi'):\n    return x\n";
        let fms = collect_file_mutations(source_single);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("single-quoted 'hi' must produce default_arg mutation");
        assert_eq!(m.replacement, "'XXhiXX'", "single-quoted must get XX-wrapped replacement");

        // Triple-quoted: must fall back to None, not produce XX-wrapping
        // Second `&&` → `||` makes condition: `(A && B) || C` where C=`len>=2` is always true,
        // so triple-quoted would enter the string branch and produce '''XXhiXX''' instead.
        let source_triple = "def f(x='''hi'''):\n    return x\n";
        let fms2 = collect_file_mutations(source_triple);
        let m2 = fms2[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("triple-quoted '''hi''' must produce a default_arg mutation (fallback to None)");
        assert_eq!(
            m2.replacement, "None",
            "triple-quoted must get fallback 'None', not '''XXhiXX'''"
        );
    }
}

// --- Keyword swap tests (break→return, continue→break) ---
#[cfg(test)]
mod keyword_swap_tests {
    use super::*;

    // INV-1: `while True: break` → break is replaced with continue.
    #[test]
    fn test_break_to_continue() {
        let source = "def f():\n    while True:\n        break\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should collect mutations from function");
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "break inside while should produce a keyword_swap mutation");
        let m = kw[0];
        assert_eq!(m.original, "break", "original must be 'break'");
        assert_eq!(m.replacement, "continue", "replacement must be 'continue'");
    }

    // INV-2: `for x in y: continue` → continue is replaced with break.
    #[test]
    fn test_continue_to_break() {
        let source = "def f(y):\n    for x in y:\n        continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "continue inside for should produce a keyword_swap mutation");
        let m = kw[0];
        assert_eq!(m.original, "continue", "original must be 'continue'");
        assert_eq!(m.replacement, "break", "replacement must be 'break'");
    }

    // INV-3: `break` inside nested if is still found.
    #[test]
    fn test_break_inside_nested_if() {
        let source = "def f(cond):\n    while True:\n        if cond:\n            break\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "break inside nested if should still produce keyword_swap");
        assert_eq!(kw[0].original, "break");
        assert_eq!(kw[0].replacement, "continue");
    }

    // INV-3: Loop with both break and continue generates 2 keyword_swap mutations.
    #[test]
    fn test_break_and_continue_both_swapped() {
        let source = "def f(items, cond):\n    for x in items:\n        if cond:\n            break\n        else:\n            continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert_eq!(kw.len(), 2, "loop with break and continue must produce 2 keyword_swap mutations");
        let originals: Vec<&str> = kw.iter().map(|m| m.original.as_str()).collect();
        assert!(originals.contains(&"break"), "must have break mutation");
        assert!(originals.contains(&"continue"), "must have continue mutation");
        for m in &kw {
            if m.original == "break" {
                assert_eq!(m.replacement, "continue");
            } else {
                assert_eq!(m.replacement, "break");
            }
        }
    }

    // INV-3: Nested loops generate keyword_swap mutations at each nesting level independently.
    #[test]
    fn test_break_continue_nested_loops() {
        let source = "def f(outer, inner):\n    for x in outer:\n        break\n        for y in inner:\n            continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert_eq!(kw.len(), 2, "nested loops with break and continue must produce 2 keyword_swap mutations");
        // Verify each is at a distinct position
        assert_ne!(kw[0].start, kw[1].start, "break and continue must be at distinct positions");
    }

    // INV-4: All keyword_swap mutations produce valid Python (parse_module succeeds).
    #[test]
    fn test_keyword_swap_parseable() {
        let sources = [
            "def f():\n    while True:\n        break\n",
            "def f(y):\n    for x in y:\n        continue\n",
            "def f(cond):\n    while True:\n        if cond:\n            break\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "keyword_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "keyword_swap mutation {:?} → {:?} produced unparseable Python:\n{}",
                        m.original, m.replacement, mutated
                    );
                }
            }
        }
    }

    // INV-5: Mutation start/end match the keyword position in source.
    #[test]
    fn test_keyword_swap_span_correctness() {
        let cases = [
            ("def f():\n    while True:\n        break\n", "break"),
            ("def f(y):\n    for x in y:\n        continue\n", "continue"),
        ];
        for (source, keyword) in &cases {
            let fms = collect_file_mutations(source);
            assert!(!fms.is_empty());
            let fm = &fms[0];
            let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
            assert!(!kw.is_empty(), "expected keyword_swap for '{keyword}'");
            let m = kw[0];
            // start..end must index the keyword in the function source
            assert_eq!(
                &fm.source[m.start..m.end], *keyword,
                "source[{}..{}] must equal '{keyword}'",
                m.start, m.end
            );
            // start < end invariant
            assert!(m.start < m.end, "start must be < end");
            // end in bounds
            assert!(m.end <= fm.source.len(), "end must be <= source length");
        }
    }
}

#[cfg(test)]
mod return_value_tests {
    use super::*;

    fn return_value_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "return_value")
    }

    // INV-1: `return a + b` → mutation replaces "a + b" with "None"
    #[test]
    fn test_return_expr_to_none() {
        let source = "def f(a, b):\n    return a + b\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return expr must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "a + b");
    }

    // INV-2: `return None` → mutation replaces "None" with `""`
    #[test]
    fn test_return_none_to_empty_string() {
        let source = "def f():\n    return None\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return None must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "\"\"");
        assert_eq!(muts[0].original, "None");
    }

    // INV-3: `return 42` → mutation replaces "42" with "None"
    #[test]
    fn test_return_constant_to_none() {
        let source = "def f():\n    return 42\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return 42 must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "42");
    }

    // INV-4: `return "hello"` → mutation replaces `"hello"` with "None"
    #[test]
    fn test_return_string_to_none() {
        let source = "def f():\n    return \"hello\"\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return string must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "\"hello\"");
    }

    // INV-5: bare `return` (no value) → no return_value mutation
    #[test]
    fn test_bare_return_no_mutation() {
        // bare return needs something else to produce a mutation so the function is collected
        let source = "def f(a, b):\n    if a > b:\n        return\n    return a + b\n";
        let muts = return_value_mutations(source);
        // Should get exactly one return_value mutation (from `return a + b`), not from bare `return`
        assert_eq!(muts.len(), 1, "bare return must not emit a return_value mutation");
        assert_eq!(muts[0].original, "a + b");
    }

    // INV-6: `return a + b` → produces BOTH return_value AND binop_swap mutations
    #[test]
    fn test_return_value_coexists_with_binop() {
        let source = "def f(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let rv: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "return_value").collect();
        let binop: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!rv.is_empty(), "must have return_value mutation");
        assert!(!binop.is_empty(), "must also have binop_swap mutation");
    }

    // INV-7: All return_value mutations produce syntactically valid Python
    #[test]
    fn test_return_value_parseable() {
        let cases = [
            "def f(a, b):\n    return a + b\n",
            "def f():\n    return None\n",
            "def f():\n    return 42\n",
            "def f():\n    return \"hello\"\n",
            "def f(a, b):\n    return a and b\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "return_value") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "return_value mutation '{}' → '{}' produced unparseable Python:\n{}",
                        m.original,
                        m.replacement,
                        mutated
                    );
                }
            }
        }
    }

    // INV-8: Mutation span covers only the value, not the `return` keyword
    #[test]
    fn test_return_value_span_correctness() {
        let source = "def f(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let m = fm.mutations.iter().find(|m| m.operator == "return_value").unwrap();

        // The span text must equal the original
        super::assert_span_matches_original(fm, m);

        // The span must NOT include "return"
        let before_span = &fm.source[..m.start];
        assert!(
            before_span.ends_with("return "),
            "the text before the value span must end with 'return ', got: {:?}",
            before_span
        );
    }

    // =====================================================================
    // Blanket decorator skip tests
    // =====================================================================
    // INV-1: Any decorated function produces NO mutations (blanket skip).
    #[test]
    fn test_non_descriptor_decorated_function_skipped() {
        let cases = [
            "@cache\ndef f():\n    return 1\n",
            "@a\n@b\ndef f():\n    return 1\n",
            "@app.route(\"/path\")\ndef f():\n    return 1\n",
            "@abstractmethod\ndef f():\n    return 1\n",
            "@override\ndef f():\n    return 1\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            assert!(
                fms.is_empty(),
                "non-descriptor decorated function must be skipped; source:\n{source}"
            );
        }
    }

    #[test]
    fn test_descriptor_decorated_functions_collected() {
        // @property, @classmethod, @staticmethod should produce mutations.
        let cases = [
            ("class C:\n    @property\n    def x(self):\n        return 1\n", "property"),
            ("class C:\n    @classmethod\n    def make(cls):\n        return 1\n", "classmethod"),
            ("class C:\n    @staticmethod\n    def helper():\n        return 1\n", "staticmethod"),
        ];
        for (source, kind) in &cases {
            let fms = collect_file_mutations(source);
            assert!(
                !fms.is_empty(),
                "@{kind} function must produce mutations; source:\n{source}"
            );
        }
    }
}

// --- Conditional split/rsplit method swap tests ---
#[cfg(test)]
mod split_swap_tests {
    use super::*;

    #[test]
    fn test_split_with_maxsplit_mutated() {
        let source = "def foo(s):\n    return s.split(\",\", 1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("split with 2 positional args must produce a method_swap mutation");
        assert_eq!(m.original, "split");
        assert_eq!(m.replacement, "rsplit");
    }

    #[test]
    fn test_rsplit_with_maxsplit_mutated() {
        let source = "def foo(s):\n    return s.rsplit(\",\", 1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("rsplit with 2 positional args must produce a method_swap mutation");
        assert_eq!(m.original, "rsplit");
        assert_eq!(m.replacement, "split");
    }

    #[test]
    fn test_split_with_maxsplit_kwarg() {
        let source = "def foo(s):\n    return s.split(\",\", maxsplit=1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("split with maxsplit kwarg must produce a method_swap mutation");
        assert_eq!(m.original, "split");
        assert_eq!(m.replacement, "rsplit");
    }

    #[test]
    fn test_rsplit_with_maxsplit_kwarg() {
        let source = "def foo(s):\n    return s.rsplit(\",\", maxsplit=1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("rsplit with maxsplit kwarg must produce a method_swap mutation");
        assert_eq!(m.original, "rsplit");
        assert_eq!(m.replacement, "split");
    }

    // INV: split/rsplit with exactly 1 positional arg and no maxsplit kwarg must NOT produce
    // a method_swap mutation — without maxsplit the two calls are semantically identical.
    #[test]
    fn test_split_one_arg_not_mutated() {
        let source = "def foo(s):\n    return s.split(\",\")\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), "split with 1 arg must not produce a method_swap mutation");
    }

    #[test]
    fn test_split_no_args_not_mutated() {
        let source = "def foo(s):\n    return s.split()\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), "split with no args must not produce a method_swap mutation");
    }

    // INV: split/rsplit mutation span is structurally correct — character before start is '.'.
    #[test]
    fn test_split_swap_span_correctness() {
        let cases = [
            "def foo(s):\n    return s.split(\",\", 1)\n",
            "def foo(s):\n    return s.rsplit(\",\", 1)\n",
            "def foo(s):\n    return s.split(\",\", maxsplit=1)\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    if m.operator == "method_swap" && (m.original == "split" || m.original == "rsplit") {
                        assert_eq!(
                            &fm.source[m.start..m.end],
                            m.original,
                            "span must cover the method name in {:?}",
                            source
                        );
                        assert!(m.start > 0, "method_swap start must be > 0");
                        assert_eq!(
                            fm.source.as_bytes()[m.start - 1],
                            b'.',
                            "character before method span must be a dot in {:?}",
                            source
                        );
                    }
                }
            }
        }
    }

    // INV: apply_mutation on a split/rsplit swap produces syntactically valid Python
    // (i.e., only the method name changes, all parens and args are preserved).
    #[test]
    fn test_split_swap_parseable() {
        let cases = [
            ("def foo(s):\n    return s.split(\",\", 1)\n", "split", "rsplit"),
            ("def foo(s):\n    return s.rsplit(\",\", 1)\n", "rsplit", "split"),
        ];
        for (source, original, replacement) in &cases {
            let fms = collect_file_mutations(source);
            let m = fms[0]
                .mutations
                .iter()
                .find(|m| m.operator == "method_swap" && m.original == *original)
                .expect("must find method_swap mutation");
            let mutated = apply_mutation(&fms[0].source, m);
            assert!(
                mutated.contains(replacement),
                "mutated source must contain replacement method name {:?}: got {:?}",
                replacement,
                mutated
            );
            // The parens and arguments must still be present.
            assert!(
                mutated.contains("(\",\", 1)"),
                "mutated source must preserve call arguments: got {:?}",
                mutated
            );
        }
    }
}

#[cfg(test)]
mod dict_kwarg_tests {
    use super::*;

    fn kwarg_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "dict_kwarg")
    }

    #[test]
    fn test_dict_single_kwarg() {
        let source = "def f():\n    return dict(a=1)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 1, "dict(a=1) must produce exactly one dict_kwarg mutation");
        assert_eq!(muts[0].original, "a");
        assert_eq!(muts[0].replacement, "aXX");
    }

    #[test]
    fn test_dict_multiple_kwargs() {
        let source = "def f():\n    return dict(a=1, b=2)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 2, "dict(a=1, b=2) must produce two dict_kwarg mutations");
        let originals: Vec<&str> = muts.iter().map(|m| m.original.as_str()).collect();
        assert!(originals.contains(&"a"), "must mutate kwarg 'a'");
        assert!(originals.contains(&"b"), "must mutate kwarg 'b'");
        assert_eq!(muts.iter().find(|m| m.original == "a").unwrap().replacement, "aXX");
        assert_eq!(muts.iter().find(|m| m.original == "b").unwrap().replacement, "bXX");
    }

    #[test]
    fn test_dict_no_kwargs() {
        let source = "def f():\n    return dict()\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "dict() must produce no dict_kwarg mutations");
    }

    #[test]
    fn test_dict_positional_only() {
        let source = "def f():\n    return dict([(1, 2)])\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "dict with positional-only args must not produce dict_kwarg mutations");
    }

    #[test]
    fn test_dict_mixed_args() {
        // dict(a=1, **extra) — only `a` is a plain keyword arg; **extra is starred
        let source = "def f(extra):\n    return dict(a=1, **extra)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 1, "only plain kwarg 'a' must be mutated, not **extra");
        assert_eq!(muts[0].original, "a");
        assert_eq!(muts[0].replacement, "aXX");
    }

    #[test]
    fn test_non_dict_call_no_mutation() {
        // foo(a=1) must NOT produce dict_kwarg mutations — only dict() calls are targeted.
        let source = "def f():\n    foo(a=1)\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "foo(a=1) must not produce dict_kwarg mutations");
    }

    #[test]
    fn test_dict_kwarg_parseable() {
        // Verify that applying all dict_kwarg mutations produces valid (parseable) Python.
        let source = "def f():\n    return dict(foo=1, bar=2)\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in fm.mutations.iter().filter(|m| m.operator == "dict_kwarg") {
                let mutated = apply_mutation(&fm.source, m);
                // A mutated source is parseable if libcst can collect mutations from it.
                // We only need to verify that collect_file_mutations doesn't panic.
                let _ = collect_file_mutations(&mutated);
            }
        }
    }

    #[test]
    fn test_dict_kwarg_span_correctness() {
        // INV-3: fm.source[m.start..m.end] must equal m.original for dict_kwarg mutations.
        let source = "def f():\n    return dict(foo=1, bar=2)\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in fm.mutations.iter().filter(|m| m.operator == "dict_kwarg") {
                let slice = &fm.source[m.start..m.end];
                assert_eq!(
                    slice, m.original.as_str(),
                    "source slice at [{}..{}] must equal original '{}', got '{}'",
                    m.start, m.end, m.original, slice
                );
            }
        }
    }
}

#[cfg(test)]
mod exception_type_tests {
    use super::*;

    fn exception_type_mutations_for(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                let pairs: Vec<_> = fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "exception_type")
                    .map(|m| (fm.clone(), m.clone()))
                    .collect();
                pairs
            })
            .collect()
    }

    #[test]
    fn test_except_valueerror_to_exception() {
        let source = "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
    }

    #[test]
    fn test_except_tuple_to_exception() {
        let source =
            "def f():\n    try:\n        pass\n    except (TypeError, ValueError):\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected for tuple type");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "(TypeError, ValueError)");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "(TypeError, ValueError)");
    }

    #[test]
    fn test_except_exception_no_mutation() {
        // `except Exception:` is already the broadest type — no mutation should be emitted.
        let source = "def f():\n    try:\n        pass\n    except Exception:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 0, "except Exception must not produce an exception_type mutation");
    }

    #[test]
    fn test_bare_except_no_mutation() {
        // Bare `except:` has no type field — nothing to broaden.
        let source = "def f():\n    try:\n        pass\n    except:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 0, "bare except must not produce an exception_type mutation");
    }

    #[test]
    fn test_except_with_as_binding() {
        // `except ValueError as e:` — mutation targets only the type, not the `as e` binding.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError as e:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
        // The character immediately after the type span must be a space (before `as`).
        assert_eq!(
            fm.source.as_bytes()[m.end],
            b' ',
            "char after type span must be space (before 'as')"
        );
    }

    #[test]
    fn test_multiple_handlers() {
        // One mutation per typed handler; both TypeError and ValueError should be mutated.
        let source = concat!(
            "def f():\n",
            "    try:\n",
            "        pass\n",
            "    except TypeError:\n",
            "        pass\n",
            "    except ValueError:\n",
            "        pass\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "two typed handlers must produce two exception_type mutations");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"TypeError"), "TypeError handler must be mutated");
        assert!(originals.contains(&"ValueError"), "ValueError handler must be mutated");
    }

    #[test]
    fn test_exception_type_parseable() {
        // After mutation, the function source must still parse as valid Python.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let exc_m = fm
            .mutations
            .iter()
            .find(|m| m.operator == "exception_type")
            .expect("must have an exception_type mutation");
        let mutated = apply_mutation(&fm.source, exc_m);
        assert!(
            parse_module(&mutated, None).is_ok(),
            "mutated source must be parseable: {mutated}"
        );
    }

    #[test]
    fn test_exception_type_span_correctness() {
        // INV-3: fm.source[m.start..m.end] must equal m.original for exception_type mutations.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let exc_m = fm
            .mutations
            .iter()
            .find(|m| m.operator == "exception_type")
            .expect("must have an exception_type mutation");
        assert_eq!(
            &fm.source[exc_m.start..exc_m.end],
            exc_m.original.as_str(),
            "source slice must equal mutation original"
        );
    }

    #[test]
    fn test_bare_then_typed_handler() {
        // Bare except in one try block, typed except in a separate try block.
        // The bare except cursor advance must not discard the typed handler in the second block.
        // Since each try block calls add_exception_type_mutations independently, the cursor for
        // the second block (cursor_before_2) is derived from the structural cursor after the
        // first block. Exactly 1 exception_type mutation (on ValueError) must be emitted.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except:\n",
            "        pass\n",
            "    try:\n",
            "        return x + 2\n",
            "    except ValueError:\n",
            "        return 0\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "exactly one exception_type mutation expected (from the second try block)");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
    }

    #[test]
    fn test_two_typed_handlers() {
        // Two typed handlers in the same try block — one for ValueError, one for TypeError.
        // Each must produce an independent exception_type mutation.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except ValueError:\n",
            "        return 0\n",
            "    except TypeError:\n",
            "        return -1\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "exactly two exception_type mutations expected");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"ValueError"), "ValueError must be mutated");
        assert!(originals.contains(&"TypeError"), "TypeError must be mutated");
        // Each mutation must point to a distinct position in the source.
        assert_ne!(pairs[0].1.start, pairs[1].1.start, "mutations must target different source positions");
        for (fm, m) in &pairs {
            super::assert_span_matches_original(fm, m);
        }
    }

    #[test]
    fn test_three_handlers_mixed() {
        // Three typed handlers — ValueError, TypeError, Exception.
        // Exception is already the broadest type and must be skipped.
        // Exactly 2 exception_type mutations must be emitted (for ValueError and TypeError).
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x\n",
            "    except ValueError:\n",
            "        return 1\n",
            "    except TypeError:\n",
            "        return 2\n",
            "    except Exception:\n",
            "        return 3\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "Exception handler must be skipped; exactly 2 mutations expected");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"ValueError"), "ValueError must be mutated");
        assert!(originals.contains(&"TypeError"), "TypeError must be mutated");
        assert!(!originals.contains(&"Exception"), "Exception must not be mutated");
        // Mutations must target distinct, increasing positions (cursor advances forward).
        assert!(
            pairs[0].1.start < pairs[1].1.start,
            "mutations must be ordered by source position"
        );
        for (fm, m) in &pairs {
            super::assert_span_matches_original(fm, m);
        }
    }

    #[test]
    fn test_duplicate_handlers_distinct_positions() {
        // Two handlers of the same exception type in the same try block.
        // Python allows this (the second is unreachable); libcst parses it fine.
        // The sub-cursor must advance PAST the first handler before searching for the second,
        // so both mutations must point to distinct positions.
        // Regression: if cursor goes backward after the first handler, it re-finds the first
        // handler for the second — both mutations collapse to the same span.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except ValueError:\n",
            "        return 0\n",
            "    except ValueError:\n",
            "        return -1\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "two exception_type mutations expected (one per handler)");
        // The two mutations must point to different byte offsets in the source.
        assert_ne!(
            pairs[0].1.start,
            pairs[1].1.start,
            "cursor must advance past first handler before searching for second (distinct positions required)"
        );
        for (fm, m) in &pairs {
            super::assert_span_matches_original(fm, m);
        }
    }

    // --- condition_negation tests ---

    // INV-1: Applying any condition_negation mutation must produce parseable Python.
    // INV-2: Operator name is always "condition_negation".
    // INV-3: Replacement is always `not ({original_condition})`.

    fn condition_negation_mutations_for(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                fm.mutations
                    .iter()
                    .filter(|m| m.operator == "condition_negation")
                    .cloned()
                    .map(|m| (fm.clone(), m))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn test_condition_negation_if_statement_ordinal_skipped() {
        // `if x > 0:` has an ordinal comparison → condition_negation is skipped
        // (subsumed by Kaminski compop_swap mutations).
        let source = "def f(x):\n    if x > 0:\n        return x\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "ordinal comparison → condition_negation skipped (Kaminski)");
    }

    #[test]
    fn test_condition_negation_if_non_comparison() {
        // `if items:` is not a comparison → condition_negation IS emitted.
        let source = "def f(items):\n    if items:\n        return items[0]\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "non-comparison condition → condition_negation emitted");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "items");
        assert_eq!(m.replacement, "not (items)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok());
    }

    #[test]
    fn test_condition_negation_while_loop() {
        // Critical path: `while items:` generates one mutation.
        let source = "def f(items):\n    while items:\n        items.pop()\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for a single while");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "items");
        assert_eq!(m.replacement, "not (items)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_assert_no_message() {
        // Critical path: `assert result == expected` → condition mutated, no msg.
        let source = "def f(result, expected):\n    assert result == expected\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for assert without message");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "result == expected");
        assert_eq!(m.replacement, "not (result == expected)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_assert_with_message() {
        // Critical path: `assert cond, "msg"` — mutation targets condition only, message preserved.
        let source = "def f(result):\n    assert result, \"expected true\"\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for assert with message");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "result");
        assert_eq!(m.replacement, "not (result)");
        // The mutated function must still include the message.
        let mutated = apply_mutation(&fm.source, m);
        assert!(mutated.contains("\"expected true\""), "message must be preserved in mutated source");
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_ternary_expression() {
        // Critical path: `x if flag else y` → `x if not (flag) else y`
        let source = "def f(x, y, flag):\n    return x if flag else y\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for ternary");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "flag");
        assert_eq!(m.replacement, "not (flag)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_compound_condition() {
        // Compound: `if a and b or c:` → `if not (a and b or c):`
        let source = "def f(a, b, c):\n    if a and b or c:\n        return 1\n";
        let pairs = condition_negation_mutations_for(source);
        let cn: Vec<_> = pairs.iter().filter(|(_, m)| m.operator == "condition_negation").collect();
        assert_eq!(cn.len(), 1, "one condition_negation for compound condition");
        let (fm, m) = &cn[0];
        assert_eq!(m.replacement, format!("not ({})", m.original));
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_already_negated_skipped() {
        // Failure mode: `if not x:` must NOT generate condition_negation (unary_removal covers it).
        let source = "def f(x):\n    if not x:\n        return 1\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "condition_negation must be skipped when condition is already `not <expr>`");
    }

    #[test]
    fn test_condition_negation_elif_branch_ordinal_skipped() {
        // Both `x > 0` and `x < 0` are ordinal comparisons → condition_negation skipped.
        let source = "def f(x):\n    if x > 0:\n        return 1\n    elif x < 0:\n        return -1\n    else:\n        return 0\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "ordinal comparisons → condition_negation skipped (Kaminski)");
    }

    #[test]
    fn test_condition_negation_multi_elif_chain_ordinal_skipped() {
        // All conditions are ordinal comparisons (>) → all skipped.
        let source = concat!(
            "def f(x):\n",
            "    if x > 10:\n",
            "        return 'high'\n",
            "    elif x > 5:\n",
            "        return 'mid'\n",
            "    elif x > 0:\n",
            "        return 'low'\n",
            "    else:\n",
            "        return 'neg'\n",
        );
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "ordinal comparisons → condition_negation skipped (Kaminski)");
    }

    #[test]
    fn test_condition_negation_mixed_ordinal_and_non() {
        // Mix of ordinal comparison and non-comparison conditions.
        let source = concat!(
            "def f(x, items):\n",
            "    if x > 10:\n",
            "        return 'high'\n",
            "    elif items:\n",
            "        return 'has items'\n",
        );
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "ordinal skipped, non-comparison emitted");
        assert_eq!(pairs[0].1.original, "items");
    }

    #[test]
    fn test_condition_negation_five_elif_chain_ordinal_skipped() {
        // All conditions are ordinal comparisons (==) → all skipped.
        let source = concat!(
            "def f(x):\n",
            "    if x == 1:\n",
            "        return 'one'\n",
            "    elif x == 2:\n",
            "        return 'two'\n",
            "    elif x == 3:\n",
            "        return 'three'\n",
            "    elif x == 4:\n",
            "        return 'four'\n",
            "    elif x == 5:\n",
            "        return 'five'\n",
            "    else:\n",
            "        return 'other'\n",
        );
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "ordinal comparisons (==) → condition_negation skipped (Kaminski)");
    }

    #[test]
    fn test_condition_negation_elif_bodies_not_treated_as_conditions() {
        // INV-3: assignment targets inside elif bodies must NOT get condition_negation.
        // This is the specific failure mode from bench/corpora/click/src/click/core.py.
        let source = concat!(
            "def f(x):\n",
            "    if isinstance(x, list):\n",
            "        pass\n",
            "    elif isinstance(x, range):\n",
            "        pass\n",
            "    elif isinstance(x, str):\n",
            "        result = x.upper()\n",
            "    else:\n",
            "        result = str(x)\n",
        );
        let pairs = condition_negation_mutations_for(source);
        // Exactly 3 condition_negation mutations (if + 2 elifs), none on assignment targets
        assert_eq!(pairs.len(), 3, "if + 2 elifs = 3 condition_negation mutations");
        for (_, m) in &pairs {
            // original must be the test expression, never an assignment target like "result"
            assert!(
                !m.original.contains('='),
                "condition_negation must not target assignment: got '{}'",
                m.original
            );
            assert!(
                m.original.starts_with("isinstance"),
                "condition must be isinstance call, got '{}'",
                m.original
            );
        }
    }

    #[test]
    fn test_condition_negation_elif_terminal_else_processed() {
        // INV-4: the final else body after elifs must produce mutations.
        let source = concat!(
            "def f(x, y):\n",
            "    if x > 0:\n",
            "        return x\n",
            "    elif x < 0:\n",
            "        return -x\n",
            "    else:\n",
            "        return y + 1\n",  // binop_swap should fire here
        );
        // Filter for all mutations (not just condition_negation) and verify the else body is reached
        let all_muts: Vec<(FunctionMutations, Mutation)> = collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                fm.mutations
                    .iter()
                    .map(|m| (fm.clone(), m.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        // The else body has `y + 1` — binop_swap should produce a mutation there
        let else_body_muts: Vec<_> = all_muts
            .iter()
            .filter(|(_, m)| m.operator == "binop_swap" && m.original.contains('+'))
            .collect();
        assert!(!else_body_muts.is_empty(), "else body after elif chain must be mutated");
    }

    #[test]
    fn test_condition_negation_nested_if() {
        // Nested if inside if — both conditions get independent mutations.
        let source = "def f(a, b):\n    if a:\n        if b:\n            return 1\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 2, "outer and inner if each get one condition_negation");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"a"), "outer if condition must be mutated");
        assert!(originals.contains(&"b"), "inner if condition must be mutated");
        for (fm, m) in &pairs {
            let mutated = apply_mutation(&fm.source, m);
            assert!(parse_module(&mutated, None).is_ok(), "mutated source must parse: {mutated}");
        }
    }

    #[test]
    fn test_condition_negation_parseability_all_sites() {
        // INV-1: every condition_negation mutation must produce parseable Python across all sites.
        // Note: ordinal comparisons in if/while are skipped (Kaminski), but assert and
        // ternary use separate codepaths and still emit condition_negation.
        let cases = [
            // Non-comparison conditions in if/while still get condition_negation:
            "def f(items):\n    while items:\n        items.pop()\n",
            "def f(r):\n    assert r, \"msg\"\n",
            "def f(x, y, flag):\n    return x if flag else y\n",
            "def f(a, b, c):\n    if a and b or c:\n        return 1\n",
            // assert with comparison — assert uses add_condition_negation_assert, not statement:
            "def f(r, e):\n    assert r == e\n",
        ];
        for source in &cases {
            let pairs = condition_negation_mutations_for(source);
            assert!(!pairs.is_empty(), "should produce at least one condition_negation for: {source}");
            for (fm, m) in &pairs {
                assert_eq!(m.operator, "condition_negation");
                assert_eq!(m.replacement, format!("not ({})", m.original), "INV-3 violated");
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "INV-1 violated: unparseable mutant for {source}: {mutated}"
                );
            }
        }
    }
}

#[cfg(test)]
mod ternary_swap_tests {
    use super::*;
    use libcst_native::parse_module;

    fn ternary_mutations(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                fm.mutations
                    .iter()
                    .filter(|m| m.operator == "ternary_swap")
                    .map(|m| (fm.clone(), m.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    // INV-2: operator name is "ternary_swap"
    #[test]
    fn test_operator_name() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        assert!(!pairs.is_empty(), "must produce at least one ternary_swap mutation");
        for (_, m) in &pairs {
            assert_eq!(m.operator, "ternary_swap");
        }
    }

    // INV-3: condition is preserved; only body and orelse are swapped
    #[test]
    fn test_simple_swap() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        let swap_muts: Vec<_> = pairs.iter().filter(|(_, m)| m.original.contains("flag")).collect();
        assert_eq!(swap_muts.len(), 1, "x if flag else y must produce exactly one ternary_swap");
        let (_, m) = &swap_muts[0];
        assert_eq!(m.original, "x if flag else y");
        assert_eq!(m.replacement, "y if flag else x", "body and orelse must be swapped; condition stays");
    }

    // INV-1: every generated mutation produces parseable Python
    #[test]
    fn test_parseable() {
        let sources = [
            "def f(ok):\n    return \"yes\" if ok else \"no\"\n",
            "def f(cond, a, b):\n    return f(a) if cond else g(b)\n",
            "def f(c1, c2):\n    return a if c1 else (b if c2 else d)\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "ternary_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "mutated source must be parseable:\n{mutated}"
                    );
                }
            }
        }
    }

    // Identical branches must NOT generate a ternary_swap (equivalent mutant)
    #[test]
    fn test_identical_branches_skipped() {
        let source = "def f(cond):\n    return x if cond else x\n";
        let pairs = ternary_mutations(source);
        assert!(pairs.is_empty(), "identical branches must not produce ternary_swap mutation");
    }

    // String literals: "yes" if ok else "no"
    #[test]
    fn test_string_branches() {
        let source = "def f(ok):\n    return \"yes\" if ok else \"no\"\n";
        let pairs = ternary_mutations(source);
        let swap_muts: Vec<_> = pairs.iter().filter(|(_, m)| m.original.contains("ok")).collect();
        assert_eq!(swap_muts.len(), 1);
        let (_, m) = &swap_muts[0];
        assert!(m.replacement.starts_with("\"no\""), "orelse must become body: {}", m.replacement);
        assert!(m.replacement.ends_with("\"yes\""), "body must become orelse: {}", m.replacement);
        assert!(m.replacement.contains("ok"), "condition must be preserved: {}", m.replacement);
    }

    // Ternary in a function call: f(a if c else b) — still generates mutation
    #[test]
    fn test_ternary_inside_call() {
        let source = "def f(c, a, b):\n    return g(a if c else b)\n";
        let pairs = ternary_mutations(source);
        assert!(!pairs.is_empty(), "ternary inside a call must still generate ternary_swap");
        let (_, m) = &pairs[0];
        assert_eq!(m.original, "a if c else b");
        assert_eq!(m.replacement, "b if c else a");
    }

    // Nested ternary: each level gets its own swap independently
    #[test]
    fn test_nested_ternary() {
        let source = "def f(c1, c2):\n    return a if c1 else (b if c2 else d)\n";
        let pairs = ternary_mutations(source);
        let swap_originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        // Outer swap
        assert!(
            swap_originals.iter().any(|s| s.contains("c1")),
            "outer ternary must be swapped; got: {swap_originals:?}"
        );
        // Inner swap
        assert!(
            swap_originals.iter().any(|s| s.contains("c2") && !s.contains("c1")),
            "inner ternary must be swapped independently; got: {swap_originals:?}"
        );
    }

    // Span correctness: fm.source[m.start..m.end] == m.original
    #[test]
    fn test_span_correctness() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        for (fm, m) in &pairs {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original.as_str(),
                "source slice at [{}..{}] must equal original '{}', got '{}'",
                m.start, m.end, m.original, slice
            );
        }
    }
}

#[cfg(test)]
mod loop_mutation_tests {
    use super::*;
    use libcst_native::parse_module;

    /// Extract all loop_mutation mutations from a source string, returning (fm, mutation) pairs.
    fn loop_mutations_for(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                let pairs: Vec<_> = fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "loop_mutation")
                    .map(|m| (fm.clone(), m.clone()))
                    .collect();
                pairs
            })
            .collect()
    }

    /// Apply a mutation to function source and verify the result parses as valid Python.
    fn mutated_source_parses(fm: &FunctionMutations, m: &Mutation) -> bool {
        let mut result = fm.source.clone();
        result.replace_range(m.start..m.end, &m.replacement);
        parse_module(&result, None).is_ok()
    }

    // INV-1 helper: all loop_mutation results must produce parseable Python.
    fn assert_all_parse(pairs: &[(FunctionMutations, Mutation)]) {
        for (fm, m) in pairs {
            assert!(
                mutated_source_parses(fm, m),
                "mutated source must parse: original='{}' replacement='{}'",
                m.original,
                m.replacement
            );
        }
    }

    // INV-2 / INV-3 helper: verify operator name and span correctness.
    fn assert_span_correct(pairs: &[(FunctionMutations, Mutation)]) {
        for (fm, m) in pairs {
            assert_eq!(m.operator, "loop_mutation", "INV-2: operator must be loop_mutation");
            assert_eq!(
                &fm.source[m.start..m.end],
                m.original.as_str(),
                "INV-3: span [{}..{}] must equal original '{}'",
                m.start,
                m.end,
                m.original
            );
        }
    }

    #[test]
    fn test_for_range_iterable_replaced_with_empty_list() {
        // INV-3: for-loop mutation replaces iterable with [].
        let source = "def f():\n    for x in range(10):\n        pass\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "exactly one loop_mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.replacement, "[]", "replacement must be []");
        assert_eq!(m.original, "range(10)", "original must be the iterable");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
        let _ = fm;
    }

    #[test]
    fn test_for_tuple_unpack_iterable_replaced() {
        // for k, v in items.items() — tuple target, attribute-call iterable.
        let source = "def f(d):\n    for k, v in d.items():\n        pass\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "exactly one loop_mutation expected");
        let (_, m) = &pairs[0];
        assert_eq!(m.replacement, "[]");
        assert_eq!(m.original, "d.items()");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_while_condition_replaced_with_false() {
        // INV-3: while-loop mutation replaces condition with False.
        let source = "def f(q):\n    while q:\n        q.pop()\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "exactly one loop_mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.replacement, "False", "replacement must be False");
        assert_eq!(m.original, "q", "original must be the condition");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
        let _ = fm;
    }

    #[test]
    fn test_while_true_generates_mutation() {
        // `while True:` → `while False:` is valid and catches real bugs.
        let source = "def f():\n    while True:\n        break\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "while True must generate one loop_mutation");
        let (_, m) = &pairs[0];
        assert_eq!(m.replacement, "False");
        assert_eq!(m.original, "True");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_async_for_generates_mutation() {
        // async for x in aiter — must generate one loop_mutation.
        let source = "async def f(aiter):\n    async for x in aiter:\n        pass\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "async for must generate one loop_mutation");
        let (_, m) = &pairs[0];
        assert_eq!(m.replacement, "[]");
        assert_eq!(m.original, "aiter");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_nested_for_loops_each_get_mutation() {
        // Each loop gets its own independent mutation.
        let source = concat!(
            "def f(outer, inner):\n",
            "    for x in outer:\n",
            "        for y in inner:\n",
            "            pass\n",
        );
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 2, "each for-loop must get its own mutation");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"outer"), "outer loop must be mutated");
        assert!(originals.contains(&"inner"), "inner loop must be mutated");
        // Distinct positions.
        assert_ne!(pairs[0].1.start, pairs[1].1.start, "mutations must target distinct positions");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_for_else_mutation_targets_iterable_only() {
        // for-else: mutation replaces the iterable, else block is preserved.
        let source = concat!(
            "def f(items):\n",
            "    for x in items:\n",
            "        pass\n",
            "    else:\n",
            "        pass\n",
        );
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one loop_mutation expected");
        let (_, m) = &pairs[0];
        assert_eq!(m.replacement, "[]");
        assert_eq!(m.original, "items");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_while_else_mutation_targets_condition_only() {
        // while-else: mutation replaces the condition, else block is preserved.
        let source = concat!(
            "def f(q):\n",
            "    while q:\n",
            "        q.pop()\n",
            "    else:\n",
            "        pass\n",
        );
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one loop_mutation expected");
        let (_, m) = &pairs[0];
        assert_eq!(m.replacement, "False");
        assert_eq!(m.original, "q");
        assert_all_parse(&pairs);
        assert_span_correct(&pairs);
    }

    #[test]
    fn test_for_already_empty_list_skipped() {
        // `for x in []: pass` — iterable is already [], skip to avoid no-op mutation.
        let source = "def f():\n    for x in []:\n        pass\n";
        let pairs = loop_mutations_for(source);
        assert_eq!(pairs.len(), 0, "for x in [] must not generate a loop_mutation (no-op)");
    }
}

#[cfg(test)]
mod statement_deletion_tests {
    use super::*;
    use libcst_native::parse_module;

    fn statement_deletion_mutations(source: &str) -> Vec<Mutation> {
        super::mutations_by_operator(source, "statement_deletion")
    }

    // INV-1: simple assignment `x = foo()` → 1 statement_deletion with replacement "pass"
    #[test]
    fn test_stmt_del_assignment() {
        let source = "def f():\n    x = foo()\n    return x\n";
        let muts = statement_deletion_mutations(source);
        let assign_del: Vec<_> = muts.iter().filter(|m| m.original.contains("x = foo()")).collect();
        assert_eq!(assign_del.len(), 1, "x = foo() must produce exactly 1 statement_deletion");
        assert_eq!(assign_del[0].replacement, "pass");
    }

    // INV-2: `return result` no longer emits statement_deletion (redundant with return_value)
    #[test]
    fn test_stmt_del_return_no_longer_emitted() {
        let source = "def f(x):\n    return x + 1\n";
        let muts = statement_deletion_mutations(source);
        let ret_del: Vec<_> = muts.iter().filter(|m| m.replacement == "return None").collect();
        assert!(ret_del.is_empty(), "return statements should not produce statement_deletion (redundant with return_value)");
    }

    // INV-3: `print(x)` → 1 statement_deletion with replacement "pass"
    #[test]
    fn test_stmt_del_expr_statement() {
        let source = "def f(x):\n    print(x)\n";
        let muts = statement_deletion_mutations(source);
        assert_eq!(muts.len(), 1, "print(x) must produce exactly 1 statement_deletion");
        assert_eq!(muts[0].replacement, "pass");
        assert!(muts[0].original.contains("print(x)"));
    }

    // INV-3: `raise ValueError("bad")` → 1 statement_deletion with replacement "pass"
    #[test]
    fn test_stmt_del_raise() {
        let source = "def f():\n    raise ValueError(\"bad\")\n";
        let muts = statement_deletion_mutations(source);
        assert_eq!(muts.len(), 1, "raise must produce exactly 1 statement_deletion");
        assert_eq!(muts[0].replacement, "pass");
        assert!(muts[0].original.starts_with("raise"));
    }

    // Failure mode: bare `return` must NOT generate statement_deletion
    #[test]
    fn test_stmt_del_bare_return_skipped() {
        let source = "def f():\n    return\n";
        let muts = statement_deletion_mutations(source);
        assert!(muts.is_empty(), "bare return must not produce statement_deletion");
    }

    // Failure mode: bare `raise` (re-raise) must NOT generate statement_deletion
    #[test]
    fn test_stmt_del_bare_raise_skipped() {
        let source = "def f():\n    try:\n        pass\n    except Exception:\n        raise\n";
        let muts = statement_deletion_mutations(source);
        assert!(muts.is_empty(), "bare raise must not produce statement_deletion");
    }

    // Failure mode: docstring expression must NOT generate statement_deletion
    #[test]
    fn test_stmt_del_docstring_skipped() {
        let source = "def f():\n    \"\"\"This is a docstring.\"\"\"\n    return 1\n";
        let muts = statement_deletion_mutations(source);
        let docstring_del: Vec<_> = muts.iter().filter(|m| m.original.contains("docstring")).collect();
        assert!(docstring_del.is_empty(), "docstring must not produce statement_deletion");
    }

    // Failure mode: `self.x = value` in __init__ must NOT generate statement_deletion
    #[test]
    fn test_stmt_del_self_assign_skipped() {
        let source = "class C:\n    def __init__(self, x):\n        self.x = x\n";
        let muts = statement_deletion_mutations(source);
        assert!(muts.is_empty(), "self.x assignment must not produce statement_deletion");
    }

    // Failure mode: augmented assign `x += 1` must NOT generate statement_deletion
    #[test]
    fn test_stmt_del_augassign_skipped() {
        let source = "def f(x):\n    x += 1\n    return x\n";
        let muts = statement_deletion_mutations(source);
        let aug_del: Vec<_> = muts.iter().filter(|m| m.original.contains("+=")).collect();
        assert!(aug_del.is_empty(), "augmented assign must not produce statement_deletion");
    }

    // Multiple eligible statements: each gets its own independent mutation.
    // return statements no longer emit statement_deletion (redundant with return_value).
    #[test]
    fn test_stmt_del_multiple_statements() {
        let source = "def f(x):\n    y = x + 1\n    print(y)\n    raise ValueError(\"e\")\n    return y\n";
        let muts = statement_deletion_mutations(source);
        assert_eq!(muts.len(), 3, "three non-return statements must produce 3 statement_deletion mutations");
    }

    // INV-1+2+3: Every generated mutation must produce parseable Python when applied
    #[test]
    fn test_stmt_del_all_produce_parseable_python() {
        let sources = [
            "def f():\n    x = foo()\n    return x\n",
            "def f(x):\n    return x + 1\n",
            "def f(x):\n    print(x)\n",
            "def f():\n    raise ValueError(\"bad\")\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "statement_deletion") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "statement_deletion '{}' → '{}' produced unparseable Python:\n{}",
                        m.original,
                        m.replacement,
                        mutated
                    );
                }
            }
        }
    }

    #[test]
    fn test_multiline_method_chain_all_mutations_parseable() {
        let source = "def f(s):\n    return (\n        s.replace(\"&\", \"&amp;\")\n        .replace(\">\", \"&gt;\")\n        .replace(\"<\", \"&lt;\")\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            let mutated = apply_mutation(&fms[0].source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "operator {} produced unparseable output:\n{}",
                m.operator,
                mutated
            );
        }
    }

    #[test]
    fn test_multiline_chain_span_correctness() {
        let source = "def f(s):\n    return (\n        s.replace(\"&\", \"&amp;\")\n        .replace(\">\", \"&gt;\")\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            assert_eq!(
                &fms[0].source[m.start..m.end],
                m.original,
                "span mismatch for operator {}",
                m.operator
            );
        }
    }

    // --- Parenthesized string literal tests (lpar/rpar offset bug) ---

    /// INV-1: source[start..end] == original for a parenthesized SimpleString.
    ///
    /// Regression test: before the fix, expr_start pointed at the opening `(`
    /// while original was just the string token, so the splice was wrong.
    #[test]
    fn test_parenthesized_string_span_correctness() {
        let source = "def f():\n    return (\n        \"hello\"\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            if m.operator == "string_mutation" || m.operator == "string_emptying" {
                let span_text = &fms[0].source[m.start..m.end];
                assert_eq!(
                    span_text, m.original,
                    "INV-1 violated for operator {}: span [{},{}) = {:?}, original = {:?}",
                    m.operator, m.start, m.end, span_text, m.original
                );
            }
        }
    }

    /// INV-2: apply_mutation on a parenthesized string produces valid Python.
    #[test]
    fn test_parenthesized_string_parseable() {
        let source = "def f():\n    return (\n        \"hello\"\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            if m.operator == "string_mutation" || m.operator == "string_emptying" {
                let mutated = apply_mutation(&fms[0].source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "operator {} produced unparseable output:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }

    /// Triggering scenario: parenthesized string as a dict value.
    /// httpx pattern: `ClientState.CLOSED: (\n    "Cannot reopen..."\n)`
    #[test]
    fn test_parenthesized_string_in_dict_value() {
        let source = "def f():\n    return {\n        \"key\": (\n            \"Cannot reopen a client\"\n        ),\n    }\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let string_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "string_mutation" || m.operator == "string_emptying")
            .collect();
        assert!(!string_muts.is_empty(), "Expected string mutations for parenthesized dict value");
        for m in &string_muts {
            // INV-1: span correctness
            let span_text = &fms[0].source[m.start..m.end];
            assert_eq!(span_text, m.original, "INV-1 violated for operator {}", m.operator);
            // INV-2: produces valid Python
            let mutated = apply_mutation(&fms[0].source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "operator {} produced unparseable output:\n{}",
                m.operator,
                mutated
            );
        }
    }

    /// Bare string (no parens) must still work — regression guard.
    #[test]
    fn test_bare_string_span_correctness_regression() {
        let source = "def f():\n    x = \"hello world\"\n    return x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            if m.operator == "string_mutation" || m.operator == "string_emptying" {
                let span_text = &fms[0].source[m.start..m.end];
                assert_eq!(span_text, m.original, "Regression: bare string span wrong for operator {}", m.operator);
            }
        }
    }

    /// Parenthesized byte-string (prefix `b`).
    #[test]
    fn test_parenthesized_bytestring_span_correctness() {
        let source = "def f():\n    return (\n        b\"bytes\"\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            if m.operator == "string_mutation" || m.operator == "string_emptying" {
                let span_text = &fms[0].source[m.start..m.end];
                assert_eq!(span_text, m.original, "INV-1 violated for b-string in parens");
                let mutated = apply_mutation(&fms[0].source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "operator {} produced unparseable output:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }

    /// INV-3: Parenthesized string produces same number of mutations as bare string.
    #[test]
    fn test_parenthesized_string_same_mutation_count() {
        let bare = "def f():\n    return \"hello\"\n";
        let parens = "def f():\n    return (\n        \"hello\"\n    )\n";
        let bare_fms = collect_file_mutations(bare);
        let parens_fms = collect_file_mutations(parens);
        let bare_count = bare_fms[0].mutations.iter().filter(|m| m.operator == "string_mutation" || m.operator == "string_emptying").count();
        let parens_count = parens_fms[0].mutations.iter().filter(|m| m.operator == "string_mutation" || m.operator == "string_emptying").count();
        assert_eq!(
            bare_count, parens_count,
            "INV-3: parenthesized string should produce same mutation count as bare string"
        );
    }

    #[test]
    fn test_multiline_return_parseable() {
        // Multi-line return with continuation
        let source = "def f(x):\n    return (\n        x + 1\n    )\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            let mutated = apply_mutation(&fms[0].source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "operator {} produced unparseable output:\n{}",
                m.operator,
                mutated
            );
        }
    }

    // --- Property descriptor skip tests ---
    // INV-1: @property-decorated method produces NO mutations.
    #[test]
    fn test_property_collected() {
        let source = "class C:\n    @property\n    def x(self):\n        return self._x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "@property method must produce mutations (descriptor-aware trampoline)");
    }

    // INV-2: @x.setter-decorated method produces NO mutations.
    #[test]
    fn test_property_setter_skipped() {
        let source = "class C:\n    @x.setter\n    def x(self, value):\n        self._x = value\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@x.setter method must not produce mutations");
    }

    // INV-3: @x.deleter-decorated method produces NO mutations.
    #[test]
    fn test_property_deleter_skipped() {
        let source = "class C:\n    @x.deleter\n    def x(self):\n        del self._x\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@x.deleter method must not produce mutations");
    }

    // INV-4: @cached_property-decorated method produces NO mutations.
    #[test]
    fn test_cached_property_skipped() {
        let source = "class C:\n    @cached_property\n    def x(self):\n        return expensive()\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@cached_property method must not produce mutations");
    }

    // INV-4b: @functools.cached_property (dotted) is also skipped via base-name match.
    #[test]
    fn test_functools_cached_property_skipped() {
        let source = "class C:\n    @functools.cached_property\n    def x(self):\n        return expensive()\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@functools.cached_property method must not produce mutations");
    }

    // INV-5: ALL decorated methods are skipped (blanket skip, not just property/setter/deleter).
    #[test]
    fn test_non_descriptor_decorated_methods_skipped() {
        // @override is not a descriptor decorator — still skipped.
        let source = "class C:\n    @override\n    def x(self):\n        return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@override method must be skipped");
    }

    #[test]
    fn test_descriptor_decorated_methods_collected() {
        // @classmethod and @staticmethod are descriptor decorators — now collected.
        let cases = [
            ("class C:\n    @classmethod\n    def make(cls):\n        return 1 + 2\n", "@classmethod"),
            ("class C:\n    @staticmethod\n    def helper():\n        return 1 + 2\n", "@staticmethod"),
        ];
        for (source, dec) in &cases {
            let fms = collect_file_mutations(source);
            assert!(!fms.is_empty(), "{dec} method must produce mutations (descriptor-aware trampoline)");
        }
    }

    // Critical path: class with getter + setter + deleter — all three are skipped.
    #[test]
    fn test_property_trio_getter_collected_setter_deleter_skipped() {
        let source = concat!(
            "class HTTPError(Exception):\n",
            "    @property\n",
            "    def request(self):\n",
            "        return self._request\n",
            "\n",
            "    @request.setter\n",
            "    def request(self, value):\n",
            "        self._request = value\n",
            "\n",
            "    @request.deleter\n",
            "    def request(self):\n",
            "        del self._request\n",
        );
        let fms = collect_file_mutations(source);
        // @property getter is collected (descriptor-aware trampoline).
        // @request.setter and @request.deleter are still skipped (not bare descriptor decorators).
        assert_eq!(fms.len(), 1, "only the @property getter should produce mutations");
        assert_eq!(fms[0].name, "request");
    }

    // Multiple decorators where one is @property — must still skip.
    #[test]
    fn test_property_with_additional_decorator_skipped() {
        let source = "class C:\n    @property\n    @some_other\n    def x(self):\n        return 1\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@property with additional decorator must still be skipped");
    }
}

// --- contextmanager / asynccontextmanager skip tests ---
#[cfg(test)]
mod contextmanager_skip_tests {
    use super::*;

    // INV-1: @contextmanager-decorated functions produce NO mutations.
    #[test]
    fn test_contextmanager_skipped() {
        let source = concat!(
            "from contextlib import contextmanager\n",
            "@contextmanager\n",
            "def request_context(request=None):\n",
            "    yield\n",
        );
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@contextmanager function must be skipped");
    }

    // INV-2: @asynccontextmanager-decorated functions produce NO mutations.
    #[test]
    fn test_asynccontextmanager_skipped() {
        let source = concat!(
            "from contextlib import asynccontextmanager\n",
            "@asynccontextmanager\n",
            "async def async_ctx():\n",
            "    yield 1 + 2\n",
        );
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@asynccontextmanager function must be skipped");
    }

    // INV-3: @contextlib.contextmanager (dotted) is also skipped.
    #[test]
    fn test_contextlib_dotted_contextmanager_skipped() {
        let source = concat!(
            "import contextlib\n",
            "@contextlib.contextmanager\n",
            "def request_context(request=None):\n",
            "    yield\n",
        );
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "@contextlib.contextmanager function must be skipped");
    }

    // INV-4: Regular generator functions (no decorator) are still mutated (regression).
    #[test]
    fn test_plain_generator_still_mutated() {
        let source = concat!(
            "def gen_values(n):\n",
            "    for i in range(n):\n",
            "        yield i + 1\n",
        );
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "plain generator function must still produce mutations");
    }
}

// --- Nonlocal detection tests ---
#[cfg(test)]
mod nonlocal_detection_tests {
    use super::*;

    // Helper: call collect_file_mutations and check whether the named function was collected.
    fn is_collected(source: &str, fn_name: &str) -> bool {
        collect_file_mutations(source).iter().any(|fm| fm.name == fn_name)
    }

    // INV-1: Function with nested function using `nonlocal` produces NO mutations.
    #[test]
    fn test_nested_nonlocal_skips_outer() {
        let source = concat!(
            "def outer(self, ctx):\n",
            "    flag = False\n",
            "    def _inner(opts):\n",
            "        nonlocal flag\n",
            "        flag = True\n",
            "    _inner([])\n",
            "    return flag + 1\n",
        );
        assert!(!is_collected(source, "outer"), "outer must be skipped because nested _inner uses nonlocal");
    }

    // INV-2: Function with nested function NOT using `nonlocal` is still mutated.
    #[test]
    fn test_nested_no_nonlocal_is_mutated() {
        let source = concat!(
            "def outer(n):\n",
            "    def inner(x):\n",
            "        return x + 1\n",
            "    return n + 1\n",
        );
        assert!(is_collected(source, "outer"), "outer without nonlocal must still be collected");
    }

    // INV-3: Function that itself uses `nonlocal` at the top level is also skipped.
    // (Normally only valid if nested, but libcst parses it at top level too.)
    #[test]
    fn test_direct_nonlocal_skips_function() {
        let source = concat!(
            "def outer():\n",
            "    nonlocal x\n",
            "    return x + 1\n",
        );
        assert!(!is_collected(source, "outer"), "function with direct nonlocal must be skipped");
    }

    // INV-4: Class method with nested function using `nonlocal` is skipped.
    #[test]
    fn test_class_method_nested_nonlocal_skipped() {
        let source = concat!(
            "class Foo:\n",
            "    def get_record(self, ctx):\n",
            "        any_slash = False\n",
            "        def _write(opts):\n",
            "            nonlocal any_slash\n",
            "            any_slash = True\n",
            "        _write([])\n",
            "        return any_slash + 1\n",
        );
        assert!(
            !is_collected(source, "get_record"),
            "class method with nested nonlocal must be skipped"
        );
    }

    // INV-5: Multiple levels of nesting — inner uses nonlocal referencing middle's var.
    #[test]
    fn test_deep_nesting_nonlocal_skips_outer() {
        let source = concat!(
            "def outer(n):\n",
            "    x = 0\n",
            "    def middle():\n",
            "        y = 0\n",
            "        def inner():\n",
            "            nonlocal y\n",
            "            y = 1\n",
            "        inner()\n",
            "        return y\n",
            "    return n + 1\n",
        );
        assert!(!is_collected(source, "outer"), "outer must be skipped when nonlocal is two levels deep");
    }

    // INV-6: `nonlocal x, y, z` (multiple names) — still detected.
    #[test]
    fn test_nonlocal_multiple_names_skips_outer() {
        let source = concat!(
            "def outer(n):\n",
            "    x = 0\n",
            "    y = 0\n",
            "    z = 0\n",
            "    def inner():\n",
            "        nonlocal x, y, z\n",
            "        x = 1\n",
            "        y = 2\n",
            "        z = 3\n",
            "    inner()\n",
            "    return n + 1\n",
        );
        assert!(!is_collected(source, "outer"), "nonlocal with multiple names must still cause outer to be skipped");
    }

    // Regression: unrelated sibling function is unaffected.
    #[test]
    fn test_sibling_function_unaffected() {
        let source = concat!(
            "def with_nonlocal():\n",
            "    x = 0\n",
            "    def inner():\n",
            "        nonlocal x\n",
            "        x = 1\n",
            "    inner()\n",
            "    return x + 1\n",
            "\n",
            "def without_nonlocal(n):\n",
            "    return n + 1\n",
        );
        assert!(!is_collected(source, "with_nonlocal"), "with_nonlocal must be skipped");
        assert!(is_collected(source, "without_nonlocal"), "without_nonlocal must still be collected");
    }
}

// --- Enum class skip tests ---
#[cfg(test)]
mod enum_skip_tests {
    use super::*;

    // Helper: check whether any function with the given name was collected from source.
    fn is_collected(source: &str, fn_name: &str) -> bool {
        collect_file_mutations(source).iter().any(|fm| fm.name == fn_name)
    }

    // INV-1: Methods inside IntEnum subclasses produce NO mutations.
    #[test]
    fn test_intenum_methods_skipped() {
        let source = concat!(
            "from enum import IntEnum\n",
            "\n",
            "class codes(IntEnum):\n",
            "    CONTINUE = 100\n",
            "    OK = 200\n",
            "\n",
            "    def __str__(self) -> str:\n",
            "        return str(self.value)\n",
            "\n",
            "    def __repr__(self) -> str:\n",
            "        return f'<{self.name}: {self.value}>'\n",
            "\n",
            "    @classmethod\n",
            "    def get_reason_phrase(cls, value: int) -> str:\n",
            "        return cls(value).name\n",
        );
        assert!(!is_collected(source, "__str__"), "IntEnum __str__ must not be mutated");
        assert!(!is_collected(source, "__repr__"), "IntEnum __repr__ must not be mutated");
        assert!(!is_collected(source, "get_reason_phrase"), "IntEnum classmethod must not be mutated");
    }

    // INV-2: Methods inside plain Enum subclasses produce NO mutations.
    #[test]
    fn test_enum_methods_skipped() {
        let source = concat!(
            "from enum import Enum\n",
            "\n",
            "class Color(Enum):\n",
            "    RED = 1\n",
            "    GREEN = 2\n",
            "\n",
            "    def describe(self) -> str:\n",
            "        return self.name + ' color'\n",
        );
        assert!(!is_collected(source, "describe"), "Enum method must not be mutated");
    }

    // INV-3: Methods inside regular (non-Enum) classes are still mutated (regression check).
    #[test]
    fn test_regular_class_still_mutated() {
        let source = concat!(
            "class Foo:\n",
            "    def bar(self, x: int) -> int:\n",
            "        return x + 1\n",
        );
        assert!(is_collected(source, "bar"), "regular class method must still be collected");
    }

    // INV-4: Both Enum and regular class in same file — only regular class is mutated.
    #[test]
    fn test_mixed_file_enum_skipped_regular_mutated() {
        let source = concat!(
            "from enum import IntEnum\n",
            "\n",
            "class Status(IntEnum):\n",
            "    OK = 200\n",
            "\n",
            "    def label(self) -> str:\n",
            "        return self.name + ' ok'\n",
            "\n",
            "class Parser:\n",
            "    def parse(self, data: str) -> int:\n",
            "        return len(data) + 1\n",
        );
        assert!(!is_collected(source, "label"), "IntEnum method must be skipped");
        assert!(is_collected(source, "parse"), "regular class method must still be collected");
    }

    // StrEnum (Python 3.11+) methods skipped.
    #[test]
    fn test_strenum_methods_skipped() {
        let source = concat!(
            "from enum import StrEnum\n",
            "\n",
            "class Direction(StrEnum):\n",
            "    NORTH = 'north'\n",
            "\n",
            "    def opposite(self) -> str:\n",
            "        return 'south' if self == 'north' else 'north'\n",
        );
        assert!(!is_collected(source, "opposite"), "StrEnum method must not be mutated");
    }

    // Flag and IntFlag skipped.
    #[test]
    fn test_flag_methods_skipped() {
        let source = concat!(
            "from enum import Flag, IntFlag\n",
            "\n",
            "class Perm(Flag):\n",
            "    READ = 1\n",
            "    WRITE = 2\n",
            "\n",
            "    def label(self) -> str:\n",
            "        return self.name or 'unknown'\n",
            "\n",
            "class Bits(IntFlag):\n",
            "    A = 1\n",
            "\n",
            "    def describe(self) -> str:\n",
            "        return f'bits={int(self)}'\n",
        );
        assert!(!is_collected(source, "label"), "Flag method must not be mutated");
        assert!(!is_collected(source, "describe"), "IntFlag method must not be mutated");
    }

    // Dotted import form: `enum.IntEnum`.
    #[test]
    fn test_dotted_enum_import_skipped() {
        let source = concat!(
            "import enum\n",
            "\n",
            "class codes(enum.IntEnum):\n",
            "    OK = 200\n",
            "\n",
            "    def label(self) -> str:\n",
            "        return self.name\n",
        );
        assert!(!is_collected(source, "label"), "enum.IntEnum method must not be mutated");
    }
}
