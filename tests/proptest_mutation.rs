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
}
