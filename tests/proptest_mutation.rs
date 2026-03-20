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
}
