//! Property-based tests for mutation engine invariants.
//!
//! These tests verify that `collect_file_mutations` and `apply_mutation` uphold
//! their documented contracts regardless of input variation.

use irradiate::mutation::{apply_mutation, collect_file_mutations};
use proptest::prelude::*;

/// Generate a single valid Python function source with a binary/boolean/comparison expression.
fn python_func_strategy() -> impl Strategy<Value = String> {
    let names = prop::sample::select(vec!["foo", "bar", "baz", "compute", "process"]);
    let params = prop::sample::select(vec!["", "x", "x, y", "a, b, c"]);
    let ops = prop::sample::select(vec!["+", "-", "*", "==", "!=", "<", ">", "and", "or"]);
    let values = prop::sample::select(vec!["0", "1", "42", "True", "False"]);

    (names, params, ops, values.clone(), values).prop_map(|(name, params, op, left, right)| {
        format!("def {name}({params}):\n    return {left} {op} {right}\n")
    })
}

/// Generate a source string with 2–3 functions to test multi-function collection.
fn multi_func_strategy() -> impl Strategy<Value = String> {
    // Use distinct name slots to avoid duplicate `def foo` etc.
    let name_a = prop::sample::select(vec!["alpha", "beta"]);
    let name_b = prop::sample::select(vec!["gamma", "delta"]);
    let name_c = prop::sample::select(vec!["epsilon", "zeta"]);
    let ops = prop::sample::select(vec!["+", "-", "*", "==", "!="]);
    let vals = prop::sample::select(vec!["0", "1", "42"]);

    (name_a, name_b, name_c, ops.clone(), ops.clone(), ops, vals.clone(), vals.clone(), vals)
        .prop_map(|(na, nb, nc, op1, op2, op3, v1, v2, v3)| {
            format!(
                "def {na}(x):\n    return x {op1} {v1}\n\ndef {nb}(x):\n    return x {op2} {v2}\n\ndef {nc}(x):\n    return x {op3} {v3}\n"
            )
        })
}

/// Arithmetic/comparison operators (always produce mutations when both operands differ).
fn arith_ops() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["+", "-", "*", "==", "!=", "<", ">"])
}

/// Integer values safe for all arithmetic operations.
fn int_vals() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["0", "1", "2", "42", "100"])
}

/// Generate functions with compound statement bodies (if/while/for/try).
///
/// Exercises mutation points inside compound statement bodies — these go through
/// different CST arms than simple `return expr` statements.
fn compound_stmt_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["if", "while", "for", "try"]);
    let op1 = arith_ops();
    let op2 = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();
    let v3 = int_vals();
    let v4 = int_vals();

    (kind, op1, op2, v1, v2, v3, v4).prop_map(|(kind, op1, op2, v1, v2, v3, v4)| match kind {
        "if" => format!(
            "def f(x):\n    if x {op1} {v1}:\n        return x {op2} {v2}\n    else:\n        return {v3} {op1} {v4}\n"
        ),
        "while" => format!(
            "def f(x):\n    while x {op1} {v1}:\n        return x {op2} {v2}\n    return {v3}\n"
        ),
        "for" => format!(
            "def f(x):\n    for i in range({v1}):\n        return i {op2} {v2}\n    return {v3}\n"
        ),
        "try" => format!(
            "def f(x):\n    try:\n        return x {op1} {v1}\n    except Exception:\n        return {v2} {op2} {v3}\n"
        ),
        _ => unreachable!(),
    })
}

/// Generate generator functions (functions containing `yield`).
///
/// Exercises the yield-detection code path and ensures `is_generator` is set correctly.
fn generator_func_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["simple", "for_loop"]);
    let op = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();

    (kind, op, v1, v2).prop_map(|(kind, op, v1, v2)| match kind {
        "simple" => format!("def f(x):\n    yield x {op} {v1}\n"),
        "for_loop" => format!(
            "def f(x):\n    for i in range({v1}):\n        yield i {op} {v2}\n"
        ),
        _ => unreachable!(),
    })
}

/// Generate functions returning container literals with operators inside.
///
/// Exercises the Tuple/List/Dict recursion arms in `collect_expr_mutations()`.
fn container_literal_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["list", "tuple", "dict"]);
    let op1 = arith_ops();
    let op2 = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();
    let v3 = int_vals();
    let v4 = int_vals();

    (kind, op1, op2, v1, v2, v3, v4).prop_map(|(kind, op1, op2, v1, v2, v3, v4)| match kind {
        "list" => format!("def f():\n    return [{v1} {op1} {v2}, {v3} {op2} {v4}]\n"),
        "tuple" => format!("def f():\n    return ({v1} {op1} {v2}, {v3} {op2} {v4})\n"),
        "dict" => format!("def f():\n    return {{\"a\": {v1} {op1} {v2}, \"b\": {v3} {op2} {v4}}}\n"),
        _ => unreachable!(),
    })
}

/// Generate functions with unary expressions.
///
/// Exercises the UnaryOperation arm and ensures mutations are found inside
/// the operand of a `not` expression.
fn unary_expr_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["not_binop", "not_cmp"]);
    let op = prop::sample::select(vec!["==", "!=", "<", ">"]);
    let v1 = int_vals();
    let v2 = int_vals();

    (kind, op, v1, v2).prop_map(|(kind, op, v1, v2)| match kind {
        "not_binop" => format!("def f(x):\n    return not (x {op} {v1})\n"),
        "not_cmp" => format!("def f(x):\n    return not ({v1} {op} {v2})\n"),
        _ => unreachable!(),
    })
}

/// Generate Python class definitions with a regular method containing a binary operator.
///
/// Exercises the class method collection path in `collect_file_mutations()`.
/// Methods like `compute`, `process`, etc. are regular (non-dunder) so they ARE mutated.
fn class_method_strategy() -> impl Strategy<Value = String> {
    let class_names = prop::sample::select(vec!["MyClass", "Foo", "Bar", "Processor"]);
    let method_names = prop::sample::select(vec!["compute", "process", "run", "execute"]);
    let ops = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();

    (class_names, method_names, ops, v1, v2).prop_map(|(cls, method, op, v1, v2)| {
        format!("class {cls}:\n    def {method}(self):\n        return {v1} {op} {v2}\n")
    })
}

/// Generate Python class definitions with ONLY a dunder method in NEVER_MUTATE_FUNCTIONS.
///
/// These methods (`__getattribute__`, `__setattr__`, `__new__`) must be skipped entirely
/// — the mutation engine must produce zero mutations for them.
fn class_dunder_method_strategy() -> impl Strategy<Value = String> {
    let class_names = prop::sample::select(vec!["MyClass", "Foo"]);
    let dunder_names =
        prop::sample::select(vec!["__getattribute__", "__setattr__", "__new__"]);
    let ops = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();

    (class_names, dunder_names, ops, v1, v2).prop_map(|(cls, method, op, v1, v2)| {
        format!("class {cls}:\n    def {method}(self):\n        return {v1} {op} {v2}\n")
    })
}

/// Generate Python functions with assignment statements.
///
/// Exercises `add_assignment_mutation_at()`:
/// - Simple: `a = x OP v` → assignment mutation (whole RHS → None)
/// - Chained: `a = b = x OP v` → value identified by summing target codegen lengths, not find("=")
/// - None assignment: `a = None` → mutated to `a = ""`
fn assignment_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["simple", "chained", "none_assign"]);
    let ops = arith_ops();
    let v1 = int_vals();

    (kind, ops, v1).prop_map(|(kind, op, v1)| match kind {
        "simple" => format!("def f(x):\n    a = x {op} {v1}\n    return a\n"),
        "chained" => format!("def f(x):\n    a = b = x {op} {v1}\n    return a\n"),
        "none_assign" => "def f():\n    a = None\n    return a\n".to_string(),
        _ => unreachable!(),
    })
}

/// Generate Python functions with match/case statements (Python 3.10+).
///
/// Exercises `add_match_case_removal_mutations()`:
/// - Each case in an N-case match (N > 1) produces exactly one removal mutation.
/// - Two-case, three-case, and wildcard variants are covered.
fn match_case_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["two_case", "three_case", "wildcard"]);

    kind.prop_map(|kind| match kind {
        "two_case" => concat!(
            "def f(x):\n",
            "    match x:\n",
            "        case 1:\n",
            "            return 1\n",
            "        case 2:\n",
            "            return 2\n",
        )
        .to_string(),
        "three_case" => concat!(
            "def f(x):\n",
            "    match x:\n",
            "        case 1:\n",
            "            return 1\n",
            "        case 2:\n",
            "            return 2\n",
            "        case 3:\n",
            "            return 3\n",
        )
        .to_string(),
        "wildcard" => concat!(
            "def f(x):\n",
            "    match x:\n",
            "        case 1:\n",
            "            return 1\n",
            "        case _:\n",
            "            return 0\n",
        )
        .to_string(),
        _ => unreachable!(),
    })
}

/// Generate functions with augmented assignment statements.
///
/// Exercises the AugAssign arm in mutation collection.
fn augassign_strategy() -> impl Strategy<Value = String> {
    let aug_op = prop::sample::select(vec!["+=", "-=", "*="]);
    let bin_op = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();

    (aug_op, bin_op, v1, v2).prop_map(|(aug_op, bin_op, v1, v2)| {
        format!("def f(x):\n    x {aug_op} {v1} {bin_op} {v2}\n    return x\n")
    })
}

/// All METHOD_SWAPS source keys — used to verify that method_swap mutation spans cover
/// only the method name (after the dot), not the object or surrounding text.
const METHOD_SWAP_KEYS: &[&str] = &[
    "lower", "upper", "lstrip", "rstrip", "find", "rfind",
    "ljust", "rjust", "index", "rindex",
    "removeprefix", "removesuffix", "partition", "rpartition",
];

/// Generate Python source containing lambda expressions inside function bodies.
///
/// Covers four variants that exercise the byte-offset splice in `add_lambda_mutation_at`:
/// - Lambda with params and a binary operator in the body (`lambda x: x + 1`)
/// - Lambda with no params and a numeric literal body (`lambda: 0`)
/// - Lambda with `None` body — mutation produces `lambda: 0` (reverse direction)
/// - Lambda where the param name appears in the body — critical regression case for the
///   byte-offset splice fix (old `String::replace()` would corrupt the param here)
///
/// Lambdas are wrapped inside a function body since `collect_file_mutations`
/// only processes top-level functions and methods.
fn lambda_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["with_params_op", "no_params", "none_body", "param_in_body"]);
    let op = arith_ops();
    let v1 = int_vals();

    (kind, op, v1).prop_map(|(kind, op, v1)| match kind {
        "with_params_op" => format!("def f():\n    g = lambda x: x {op} {v1}\n    return g(1)\n"),
        "no_params" => format!("def f():\n    g = lambda: {v1}\n    return g()\n"),
        "none_body" => "def f():\n    g = lambda: None\n    return g()\n".to_string(),
        "param_in_body" => "def f():\n    g = lambda x: x\n    return g(1)\n".to_string(),
        _ => unreachable!(),
    })
}

/// Generate Python source with method calls whose names appear in `METHOD_SWAPS`.
///
/// Includes the critical edge case where the object name equals the method name
/// (e.g. `find.find(x)`), which previously triggered a text-heuristic bug that
/// could compute the wrong byte offset for the method name.
fn method_swap_strategy() -> impl Strategy<Value = String> {
    let method = prop::sample::select(vec![
        "lower", "upper", "find", "rfind", "index", "rindex",
        "lstrip", "rstrip", "ljust", "rjust",
        "removeprefix", "removesuffix", "partition", "rpartition",
    ]);
    // "find" as object triggers the critical edge case: object name == method name.
    let obj = prop::sample::select(vec!["s", "text", "val", "find"]);

    (method, obj).prop_map(|(method, obj)| match method {
        "lower" | "upper" | "lstrip" | "rstrip" => {
            format!("def f({obj}):\n    return {obj}.{method}()\n")
        }
        _ => format!("def f({obj}, x):\n    return {obj}.{method}(x)\n"),
    })
}

/// Generate Python source with string literals of various forms.
///
/// Covers: double-quoted, single-quoted, r-prefix, b-prefix, and empty string.
/// Triple-quoted strings (docstrings) are intentionally excluded since they are
/// skipped by `add_string_mutation_at`.
fn string_literal_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["double", "single", "r_prefix", "b_prefix", "empty"]);
    let content = prop::sample::select(vec!["hello", "world", "test", "abc"]);

    (kind, content).prop_map(|(kind, content)| match kind {
        "double" => format!("def f():\n    return \"{content}\"\n"),
        "single" => format!("def f():\n    return '{content}'\n"),
        "r_prefix" => format!("def f():\n    return r\"{content}\"\n"),
        "b_prefix" => format!("def f():\n    return b\"{content}\"\n"),
        "empty" => "def f():\n    return \"\"\n".to_string(),
        _ => unreachable!(),
    })
}

/// Generate Python source with function calls containing 1, 2, or 3 positional
/// args, plus keyword args.
///
/// Used to verify `add_arg_removal_mutations`: each non-None, non-starred arg
/// produces up to 2 mutations (replace-with-None and remove-entirely).
fn arg_removal_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["one_arg", "two_args", "three_args", "keyword_arg"]);

    kind.prop_map(|kind| match kind {
        "one_arg" => "def f():\n    foo(a)\n".to_string(),
        "two_args" => "def f():\n    foo(a, b)\n".to_string(),
        "three_args" => "def f():\n    foo(a, b, c)\n".to_string(),
        "keyword_arg" => "def f():\n    foo(key=val)\n".to_string(),
        _ => unreachable!(),
    })
}

/// Generate Python functions with various return expressions.
///
/// Covers: expression with binary operator, integer constant, None literal, string literal,
/// and boolean expression — exercising `add_return_value_mutation` for all value types.
fn return_value_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["binop", "constant", "none", "string_lit", "boolop"]);
    let op = arith_ops();
    let v1 = int_vals();
    let v2 = int_vals();

    (kind, op, v1, v2).prop_map(|(kind, op, v1, v2)| match kind {
        "binop" => format!("def f(x):\n    return x {op} {v1}\n"),
        "constant" => format!("def f():\n    return {v1}\n"),
        "none" => "def f():\n    return None\n".to_string(),
        "string_lit" => format!("def f():\n    return \"{v1}\"\n"),
        "boolop" => format!("def f(a, b):\n    return a {op} {v2}\n"),
        _ => unreachable!(),
    })
}

/// Helper: check all core invariants for a set of FunctionMutations.
///
/// Used by tests that only need to verify offsets/content, not generator status.
macro_rules! assert_core_invariants {
    ($fms:expr) => {
        for fm in &$fms {
            for m in &fm.mutations {
                // INV-2: valid offsets
                prop_assert!(
                    m.start < m.end,
                    "start ({}) must be < end ({}) for {:?}",
                    m.start,
                    m.end,
                    m.operator
                );
                prop_assert!(
                    m.end <= fm.source.len(),
                    "end ({}) <= source.len() ({}) for {:?}",
                    m.end,
                    fm.source.len(),
                    m.operator
                );
                // INV-3: original matches source
                prop_assert_eq!(
                    &fm.source[m.start..m.end],
                    m.original.as_str(),
                    "source slice must equal original for {:?}",
                    m.operator
                );
                // INV-4: replacement differs
                prop_assert_ne!(
                    &m.original,
                    &m.replacement,
                    "replacement must differ from original for {:?}",
                    m.operator
                );
                // INV-5: apply_mutation length
                let mutated = apply_mutation(&fm.source, m);
                let expected_len = fm.source.len() - m.original.len() + m.replacement.len();
                prop_assert_eq!(
                    mutated.len(),
                    expected_len,
                    "length mismatch for {:?}",
                    m.operator
                );
            }
        }
    };
}

proptest! {
    /// INV-1: Determinism — collecting mutations twice yields identical results.
    #[test]
    fn deterministic(source in python_func_strategy()) {
        let first = collect_file_mutations(&source);
        let second = collect_file_mutations(&source);

        prop_assert_eq!(first.len(), second.len(), "same number of FunctionMutations");

        for (fa, fb) in first.iter().zip(second.iter()) {
            prop_assert_eq!(&fa.name, &fb.name);
            prop_assert_eq!(fa.mutations.len(), fb.mutations.len());
            for (ma, mb) in fa.mutations.iter().zip(fb.mutations.iter()) {
                prop_assert_eq!(ma.start, mb.start);
                prop_assert_eq!(ma.end, mb.end);
                prop_assert_eq!(&ma.original, &mb.original);
                prop_assert_eq!(&ma.replacement, &mb.replacement);
                prop_assert_eq!(ma.operator, mb.operator);
            }
        }
    }

    /// INV-2: Valid offsets — start < end, and end <= func source length.
    #[test]
    fn valid_offsets(source in python_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                prop_assert!(
                    m.start < m.end,
                    "start ({}) must be < end ({}) for mutation {:?}",
                    m.start, m.end, m.operator
                );
                prop_assert!(
                    m.end <= fm.source.len(),
                    "end ({}) must be <= source.len() ({}) for mutation {:?}",
                    m.end, fm.source.len(), m.operator
                );
            }
        }
    }

    /// INV-3: Original text matches source — the slice at [start..end] equals original.
    #[test]
    fn original_matches_source(source in python_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                let slice = &fm.source[m.start..m.end];
                prop_assert_eq!(
                    slice, m.original.as_str(),
                    "source[{}..{}] should equal original '{}', got '{}'",
                    m.start, m.end, m.original, slice
                );
            }
        }
    }

    /// INV-4: Replacement differs from original — mutations must actually change something.
    #[test]
    fn replacement_differs(source in python_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                prop_assert_ne!(
                    &m.original, &m.replacement,
                    "mutation {:?} must produce a different replacement",
                    m.operator
                );
            }
        }
    }

    /// INV-5: apply_mutation length — resulting string length equals the expected formula.
    #[test]
    fn apply_mutation_length(source in python_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                let expected_len = fm.source.len() - m.original.len() + m.replacement.len();
                prop_assert_eq!(
                    mutated.len(), expected_len,
                    "apply_mutation length mismatch for {:?}: got {} expected {}",
                    m.operator, mutated.len(), expected_len
                );
            }
        }
    }

    /// INV-2+3+4+5 combined over multi-function sources.
    #[test]
    fn multi_func_all_invariants(source in multi_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                // INV-2
                prop_assert!(m.start < m.end);
                prop_assert!(m.end <= fm.source.len());
                // INV-3
                prop_assert_eq!(&fm.source[m.start..m.end], m.original.as_str());
                // INV-4
                prop_assert_ne!(&m.original, &m.replacement);
                // INV-5
                let mutated = apply_mutation(&fm.source, m);
                let expected_len = fm.source.len() - m.original.len() + m.replacement.len();
                prop_assert_eq!(mutated.len(), expected_len);
            }
        }
    }

    /// Compound statement functions satisfy all core invariants (INV-2..5).
    ///
    /// Catches bugs in mutation collection inside if/while/for/try bodies where
    /// the offset cursor might reset or be miscalculated relative to the function source.
    #[test]
    fn compound_stmt_all_invariants(source in compound_stmt_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Compound statement functions produce at least one mutation.
    ///
    /// Guards against CST arms for compound statements being silently skipped,
    /// which would allow mutations inside if/while/for/try to go untested.
    #[test]
    fn compound_stmt_has_mutations(source in compound_stmt_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "compound statement function must produce at least one mutation; source:\n{source}");
    }

    /// Compound statement results are deterministic (same input → same mutations).
    #[test]
    fn compound_stmt_deterministic(source in compound_stmt_strategy()) {
        let first = collect_file_mutations(&source);
        let second = collect_file_mutations(&source);
        prop_assert_eq!(first.len(), second.len());
        for (fa, fb) in first.iter().zip(second.iter()) {
            prop_assert_eq!(fa.mutations.len(), fb.mutations.len());
            for (ma, mb) in fa.mutations.iter().zip(fb.mutations.iter()) {
                prop_assert_eq!(ma.start, mb.start);
                prop_assert_eq!(ma.end, mb.end);
                prop_assert_eq!(&ma.original, &mb.original);
                prop_assert_eq!(&ma.replacement, &mb.replacement);
            }
        }
    }

    /// Generator functions satisfy all core invariants AND are detected as generators.
    ///
    /// INV-2 (INV-generator-detection): `is_generator` must be true for any function
    /// containing a top-level `yield`. A false negative here would produce a regular
    /// function wrapper instead of a `yield from` trampoline, breaking generator callers.
    #[test]
    fn generator_detection_and_invariants(source in generator_func_strategy()) {
        let fms = collect_file_mutations(&source);

        // Every function in our generator strategy contains `yield` — all must be detected.
        for fm in &fms {
            prop_assert!(
                fm.is_generator,
                "function '{}' contains yield but is_generator=false; source:\n{source}",
                fm.name
            );
        }

        // Core invariants must hold regardless of generator status.
        assert_core_invariants!(fms);
    }

    /// Generator functions produce at least one mutation.
    ///
    /// Guards against the yield-bearing CST path silently dropping operator mutations.
    #[test]
    fn generator_has_mutations(source in generator_func_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "generator function must produce at least one mutation; source:\n{source}");
    }

    /// Container literal functions satisfy all core invariants (INV-2..5).
    ///
    /// Catches offset bugs in the List/Tuple/Dict recursion arms of
    /// `collect_expr_mutations()`, where element positions inside brackets
    /// could be miscalculated.
    #[test]
    fn container_literal_all_invariants(source in container_literal_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Container literal functions produce at least one mutation.
    ///
    /// Guards against container element expressions being silently skipped,
    /// which would mean operators inside `[a + b, c - d]` are never mutated.
    #[test]
    fn container_literal_has_mutations(source in container_literal_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "container literal function must produce at least one mutation; source:\n{source}");
    }

    /// Unary expression functions satisfy all core invariants (INV-2..5).
    ///
    /// Exercises the UnaryOperation arm — the operand of `not` contains
    /// a comparison operator that should be mutated. Offset miscalculation
    /// here would produce invalid byte spans.
    #[test]
    fn unary_expr_all_invariants(source in unary_expr_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Unary expression functions produce at least one mutation.
    ///
    /// Ensures comparison operators inside `not (a OP b)` are not silently
    /// skipped by the UnaryOperation dispatch path.
    #[test]
    fn unary_expr_has_mutations(source in unary_expr_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "unary expression function must produce at least one mutation; source:\n{source}");
    }

    /// Augmented assignment functions satisfy all core invariants (INV-2..5).
    ///
    /// Guards against the AugAssign arm computing offsets relative to the wrong
    /// base — the RHS of `x += a OP b` contains a binary operator that must
    /// produce a mutation with valid byte-span offsets.
    #[test]
    fn augassign_all_invariants(source in augassign_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Augmented assignment functions produce at least one mutation.
    ///
    /// Ensures the binary operator on the RHS of `x += a OP b` is collected
    /// and not dropped by the AugAssign dispatch path.
    #[test]
    fn augassign_has_mutations(source in augassign_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "augmented assignment function must produce at least one mutation; source:\n{source}");
    }

    // --- Class method tests ---

    /// INV-1: Class method sources satisfy all core invariants (INV-2..5).
    ///
    /// Guards against offset miscalculation when collecting mutations from methods
    /// inside a class body, where the function source is extracted differently than
    /// for top-level functions.
    #[test]
    fn class_method_all_invariants(source in class_method_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Class methods with binary operators produce at least one mutation.
    ///
    /// Guards against the class-body dispatch path silently skipping methods.
    #[test]
    fn class_method_has_mutations(source in class_method_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(
            total > 0,
            "class method with binary operator must produce mutations; source:\n{source}"
        );
    }

    /// INV-2: Class methods have class_name == Some(...).
    ///
    /// A wrong `None` here would cause the trampoline to mangle the method as a
    /// top-level function (`x_compute` instead of `xǁMyClassǁcompute`), silently
    /// producing a wrong mutant key and breaking mutant lookup.
    #[test]
    fn class_method_class_name_is_some(source in class_method_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            prop_assert!(
                fm.class_name.is_some(),
                "class method '{}' must have class_name == Some(...); source:\n{source}",
                fm.name
            );
        }
    }

    /// INV-2: Top-level functions have class_name == None.
    ///
    /// A spurious Some(...) would cause the trampoline to mangle the function with
    /// the Unicode class separator, breaking the mutant key entirely.
    #[test]
    fn top_level_func_class_name_is_none(source in python_func_strategy()) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            prop_assert!(
                fm.class_name.is_none(),
                "top-level function '{}' must have class_name == None; source:\n{source}",
                fm.name
            );
        }
    }

    /// Dunder methods in NEVER_MUTATE_FUNCTIONS must produce zero mutations.
    ///
    /// `__getattribute__`, `__setattr__`, and `__new__` are explicitly excluded
    /// because mutating them causes infinite recursion or object construction failures.
    /// A regression here would produce unsafe mutants that crash the test harness.
    #[test]
    fn class_dunder_method_no_mutations(source in class_dunder_method_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert_eq!(
            total, 0,
            "dunder methods in NEVER_MUTATE_FUNCTIONS must not be mutated; source:\n{}",
            source
        );
    }

    // --- Assignment tests ---

    /// INV-1: Assignment sources satisfy all core invariants (INV-2..5).
    ///
    /// Catches offset bugs in `add_assignment_mutation_at()` — the assignment mutation
    /// spans the whole assignment statement, and the byte span must be exact.
    #[test]
    fn assignment_all_invariants(source in assignment_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Assignment functions produce at least one mutation.
    ///
    /// Guards against assignment statements being silently skipped.  All variants in
    /// `assignment_strategy()` contain either an operator expression or a None literal,
    /// both of which must yield at least one mutation.
    #[test]
    fn assignment_has_mutations(source in assignment_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(
            total > 0,
            "assignment function must produce at least one mutation; source:\n{source}"
        );
    }

    /// INV-3: Chained assignments produce mutations with valid byte offsets.
    ///
    /// For `a = b = expr`, the value start must be computed by summing codegen lengths
    /// of all AssignTarget nodes — NOT by `find("=")`, which returns the first `=` and
    /// drops `b` as a target. The core invariant (`fm.source[m.start..m.end] == m.original`)
    /// catches this: if the offset is wrong, the source slice won't match the stored original.
    #[test]
    fn chained_assignment_core_invariants(source in
        (arith_ops(), int_vals()).prop_map(|(op, v1)| {
            format!("def f(x):\n    a = b = x {op} {v1}\n    return a\n")
        })
    ) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    // --- Match/case tests ---

    /// INV-1: Match/case sources satisfy all core invariants (INV-2..5).
    ///
    /// The match_case_removal mutations span the entire match statement (with its
    /// leading indent). Byte-span miscalculation here would produce overlapping or
    /// out-of-bounds mutations.
    #[test]
    fn match_case_all_invariants(source in match_case_strategy()) {
        let fms = collect_file_mutations(&source);
        // If libcst cannot parse match/case on this platform, skip.
        prop_assume!(!fms.is_empty());
        assert_core_invariants!(fms);
    }

    /// INV-4: An N-case match statement produces exactly N removal mutations.
    ///
    /// Catches off-by-one errors in the case-block boundary detection logic and ensures
    /// the single-case guard (`n_cases <= 1`) is not incorrectly applied.
    #[test]
    fn match_case_removal_count(source in match_case_strategy()) {
        let fms = collect_file_mutations(&source);
        // If libcst cannot parse match/case on this platform, skip.
        prop_assume!(!fms.is_empty());

        // Count `case` lines to determine expected removal count.
        let n_cases = source
            .lines()
            .filter(|l| l.trim_start().starts_with("case "))
            .count();
        prop_assume!(n_cases > 1);

        let removal_count: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "match_case_removal")
            .count();

        prop_assert_eq!(
            removal_count, n_cases,
            "N-case match must produce exactly N removal mutations; source:\n{}",
            source
        );
    }

    // --- INV-5: multi_mutation_apply meta-test ---

    /// INV-5: apply_mutation produces byte-exact prefix and suffix preservation.
    ///
    /// Verifies that for every mutation from any source type:
    ///   - `mutated[..m.start]` == `fm.source[..m.start]`  (prefix unchanged)
    ///   - `mutated[m.start..m.start + m.replacement.len()]` == `m.replacement`
    ///   - `mutated[m.start + m.replacement.len()..]` == `fm.source[m.end..]`
    ///
    /// This is stronger than length-checking: it catches bugs where apply_mutation
    /// uses the wrong start/end offsets (e.g., off-by-one) that produce the right
    /// total length but corrupt neighboring bytes.
    #[test]
    fn multi_mutation_apply_splice_exact(
        source in prop_oneof![
            python_func_strategy(),
            compound_stmt_strategy(),
            generator_func_strategy(),
            container_literal_strategy(),
            unary_expr_strategy(),
            augassign_strategy(),
            class_method_strategy(),
            assignment_strategy(),
            match_case_strategy(),
        ]
    ) {
        let fms = collect_file_mutations(&source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                // Prefix preserved
                prop_assert_eq!(
                    &mutated[..m.start],
                    &fm.source[..m.start],
                    "prefix not preserved for {:?}: source[..{}] changed",
                    m.operator, m.start
                );
                // Replacement inserted
                prop_assert_eq!(
                    &mutated[m.start..m.start + m.replacement.len()],
                    m.replacement.as_str(),
                    "replacement not found at expected position for {:?}",
                    m.operator
                );
                // Suffix preserved
                prop_assert_eq!(
                    &mutated[m.start + m.replacement.len()..],
                    &fm.source[m.end..],
                    "suffix not preserved for {:?}: source[{}..] changed",
                    m.operator, m.end
                );
            }
        }
    }

    /// Lambda functions satisfy all core invariants (INV-2..5) AND the param-preservation
    /// invariant: after mutation, the text before `:` in the lambda is unchanged.
    ///
    /// INV-3 (lambda-specific): The byte-offset splice in `add_lambda_mutation_at` must not
    /// alter text that precedes the colon separator — in particular, param names.
    /// The previous `String::replace()` implementation would corrupt params when the param
    /// name appeared in the body (e.g. `lambda x: x` → `lambda None: None` incorrectly).
    #[test]
    fn lambda_all_invariants(source in lambda_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
        // INV-3 (lambda param preservation): the text before `:` must be identical in
        // both original and replacement, ensuring params are not rewritten.
        for fm in &fms {
            for m in &fm.mutations {
                if m.operator == "lambda_mutation" {
                    let orig_colon = m.original.find(':').expect("lambda original must contain ':'");
                    let repl_colon = m.replacement.find(':').expect("lambda replacement must contain ':'");
                    prop_assert_eq!(
                        &m.original[..orig_colon],
                        &m.replacement[..repl_colon],
                        "lambda params must be unchanged after mutation; original={:?} replacement={:?}",
                        m.original,
                        m.replacement,
                    );
                }
            }
        }
    }

    /// Lambda functions produce at least one mutation.
    ///
    /// Guards against the Lambda arm in `collect_expr_mutations` being silently
    /// skipped, which would mean lambda bodies are never mutated.
    #[test]
    fn lambda_has_mutations(source in lambda_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "lambda function must produce at least one mutation; source:\n{source}");
    }

    /// Method swap sources satisfy all core invariants (INV-2..5) AND the structural
    /// invariant: every `method_swap` mutation span is immediately preceded by `.`.
    ///
    /// INV-4 (method swap): ensures the byte-offset calculation in `add_method_mutations`
    /// correctly locates the method name after the dot, even for the edge case where the
    /// object name equals the method name (e.g. `find.find(x)` — the old text-heuristic
    /// using `rfind(method_name)` instead of `rfind('.')` could find the wrong position).
    #[test]
    fn method_swap_all_invariants(source in method_swap_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
        // INV-4 (method swap span preceded by `.`):
        for fm in &fms {
            for m in &fm.mutations {
                if m.operator == "method_swap" {
                    prop_assert!(
                        METHOD_SWAP_KEYS.contains(&m.original.as_str()),
                        "method_swap original '{}' must be a known METHOD_SWAPS key",
                        m.original
                    );
                    prop_assert!(
                        m.start > 0,
                        "method_swap start ({}) must be > 0 (needs a preceding dot)",
                        m.start
                    );
                    prop_assert_eq!(
                        fm.source.as_bytes()[m.start - 1],
                        b'.',
                        "method_swap at offset {} must be preceded by '.' in source={:?}",
                        m.start,
                        &fm.source
                    );
                }
            }
        }
    }

    /// Method swap sources produce at least one mutation.
    ///
    /// Ensures the method_swap operator fires for all METHOD_SWAPS method names —
    /// a regression would silently drop the method swap, leaving that mutation path untested.
    #[test]
    fn method_swap_has_mutations(source in method_swap_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "method swap source must produce at least one mutation; source:\n{source}");
    }

    /// String literal functions satisfy all core invariants (INV-2..5).
    ///
    /// Catches delimiter-detection bugs in `add_string_mutation_at`: the original must
    /// reproduce the exact string literal (including prefix and outer quotes), and the
    /// byte-span must correctly identify the literal in the function source.
    #[test]
    fn string_literal_all_invariants(source in string_literal_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// String literal functions produce at least one mutation.
    ///
    /// Guards against string literals being silently skipped. The strategy excludes
    /// triple-quoted strings and strings where inner content contains the delimiter,
    /// so every generated source must yield a string_mutation.
    #[test]
    fn string_literal_has_mutations(source in string_literal_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "string literal function must produce at least one mutation; source:\n{source}");
    }

    /// Function calls with arg removal targets satisfy all core invariants (INV-2..5).
    #[test]
    fn arg_removal_all_invariants(source in arg_removal_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Function calls produce at least one arg_removal mutation.
    ///
    /// Ensures `add_arg_removal_mutations` fires for all arities (1-arg, 2-arg, 3-arg,
    /// keyword) — a regression would silently produce zero mutations.
    #[test]
    fn arg_removal_has_mutations(source in arg_removal_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms.iter().map(|fm| fm.mutations.len()).sum();
        prop_assert!(total > 0, "arg removal source must produce at least one mutation; source:\n{source}");
    }

    /// A 2-arg call produces at least 2 arg_removal mutations.
    ///
    /// For `foo(a, b)`, `add_arg_removal_mutations` must generate at minimum:
    ///   - `foo(None, b)` (replace arg 0 with None)
    ///   - `foo(a, None)` (replace arg 1 with None)
    /// Fewer than 2 would indicate silent arg-mutation skipping for multi-arg calls.
    #[test]
    fn arg_removal_two_arg_min_count(
        func in prop::sample::select(vec!["foo", "bar", "process"]),
        a1 in prop::sample::select(vec!["a", "x", "p"]),
        a2 in prop::sample::select(vec!["b", "y", "q"]),
    ) {
        let source = format!("def f():\n    {func}({a1}, {a2})\n");
        let fms = collect_file_mutations(&source);
        let arg_removal_count: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "arg_removal")
            .count();
        prop_assert!(
            arg_removal_count >= 2,
            "2-arg call must produce at least 2 arg_removal mutations, got {}; source:\n{source}",
            arg_removal_count
        );
    }

    // --- Return value tests ---

    /// Return value sources satisfy all core invariants (INV-2..5).
    ///
    /// Catches offset bugs in `add_return_value_mutation` — the value span must be
    /// byte-exact within the function source and the original text must match the slice.
    #[test]
    fn return_value_all_invariants(source in return_value_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Return value sources produce at least one return_value mutation.
    ///
    /// Guards against the return_value arm being silently skipped: all variants in
    /// `return_value_strategy()` have a non-bare return, so at least one mutation must fire.
    #[test]
    fn return_value_has_mutations(source in return_value_strategy()) {
        let fms = collect_file_mutations(&source);
        let total: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "return_value")
            .count();
        prop_assert!(
            total > 0,
            "return value source must produce at least one return_value mutation; source:\n{}",
            source
        );
    }

    /// INV-extra: return with a non-None value always produces exactly 1 return_value mutation.
    ///
    /// Each return statement has at most one value, so there must be exactly one return_value
    /// mutation per return-with-value.  More would indicate a double-emit bug.
    #[test]
    fn return_value_non_none_exactly_one(source in return_value_strategy()) {
        // Skip the `none` variant (returns None, which is still mutated — just to "")
        // but this test focuses on non-None values where replacement is "None".
        let fms = collect_file_mutations(&source);
        let rv_to_none: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "return_value" && m.replacement == "None")
            .count();
        let rv_to_empty: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "return_value" && m.replacement == "\"\"")
            .count();
        // There must be exactly one return_value mutation total (one return per function).
        let total_rv = rv_to_none + rv_to_empty;
        prop_assert_eq!(
            total_rv,
            1usize,
            "single return-with-value must produce exactly 1 return_value mutation; source:\n{}",
            source
        );
    }
}

/// Generate Python functions containing `break` or `continue` inside a loop body.
///
/// Exercises the keyword_swap arms in `collect_small_statement_mutations`:
/// - `while True: break` → break→return
/// - `for x in range(N): continue` → continue→break
fn keyword_swap_strategy() -> impl Strategy<Value = String> {
    let kind = prop::sample::select(vec!["while_break", "for_continue"]);
    let n = prop::sample::select(vec!["0", "1", "10", "100"]);

    (kind, n).prop_map(|(kind, n)| match kind {
        "while_break" => "def f():\n    while True:\n        break\n".to_string(),
        "for_continue" => format!("def f():\n    for x in range({n}):\n        continue\n"),
        _ => unreachable!(),
    })
}

proptest! {
    /// Keyword swap functions satisfy all core invariants (INV-2..5).
    ///
    /// Catches offset bugs in the Break/Continue arms of `collect_small_statement_mutations`:
    /// the keyword text must be located at the exact byte position recorded in the mutation.
    #[test]
    fn keyword_swap_all_invariants(source in keyword_swap_strategy()) {
        let fms = collect_file_mutations(&source);
        assert_core_invariants!(fms);
    }

    /// Keyword swap functions produce exactly 1 keyword_swap mutation.
    ///
    /// INV-extra: `break` always produces exactly 1 mutation (break→return); likewise
    /// `continue` produces exactly 1 (continue→break). More would indicate a duplicate
    /// find; fewer would indicate the arm is silently skipped.
    #[test]
    fn keyword_swap_exactly_one_mutation(source in keyword_swap_strategy()) {
        let fms = collect_file_mutations(&source);
        let kw_count: usize = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "keyword_swap")
            .count();
        prop_assert_eq!(
            kw_count, 1,
            "each loop with a single break/continue must produce exactly 1 keyword_swap mutation; source:\n{}",
            source
        );
    }
}
