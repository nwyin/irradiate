//! File-level codegen: take a Python source file and produce the fully
//! mutated version with all functions trampolined.

use crate::mutation::collect_file_mutations;
use crate::trampoline::{generate_trampoline, trampoline_impl};

/// Result of mutating a single Python source file.
#[derive(Debug)]
pub struct MutatedFile {
    /// The fully mutated source code.
    pub source: String,
    /// List of mutant keys (e.g., "module.x_func__mutmut_1").
    pub mutant_names: Vec<String>,
}

/// Generate the mutated version of a Python source file.
///
/// Returns None if no mutations were found.
pub fn mutate_file(source: &str, module_name: &str) -> Option<MutatedFile> {
    let function_mutations = collect_file_mutations(source);

    if function_mutations.is_empty() {
        return None;
    }

    let mutated_func_names: std::collections::HashSet<&str> =
        function_mutations.iter().map(|fm| fm.name.as_str()).collect();

    let mut output = String::new();
    let mut all_mutant_names = Vec::new();

    // Prepend trampoline implementation
    output.push_str(trampoline_impl());
    output.push('\n');

    // Walk through source lines, stripping out functions that will be replaced
    // by trampoline arrangements.
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        // Check if this line starts a function definition we're mutating
        let func_name = extract_func_name(trimmed);
        if !func_name.is_empty() && mutated_func_names.contains(func_name) {
            // Skip the entire function body
            let func_indent = indent;
            i += 1;
            while i < lines.len() {
                let next = lines[i];
                let next_trimmed = next.trim_start();
                let next_indent = next.len() - next_trimmed.len();
                // Function ends when we hit a non-empty line at same or lesser indent
                if !next_trimmed.is_empty() && next_indent <= func_indent {
                    break;
                }
                i += 1;
            }
            continue;
        }

        output.push_str(line);
        output.push('\n');
        i += 1;
    }

    // Append all trampoline arrangements
    output.push('\n');
    for fm in &function_mutations {
        let (trampoline_code, mutant_names) = generate_trampoline(fm, module_name);
        output.push_str(&trampoline_code);
        output.push_str("\n\n");
        all_mutant_names.extend(mutant_names);
    }

    Some(MutatedFile {
        source: output,
        mutant_names: all_mutant_names,
    })
}

fn extract_func_name(line: &str) -> &str {
    let after_def = if let Some(rest) = line.strip_prefix("async def ") {
        rest
    } else if let Some(rest) = line.strip_prefix("def ") {
        rest
    } else {
        return "";
    };

    after_def.split('(').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mutate_simple_file() {
        let source = "def add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "simple_lib").unwrap();

        assert!(result.source.contains("import irradiate_harness"), "Should have harness import");
        assert!(result.source.contains("x_add__mutmut_orig"), "Should have original renamed");
        assert!(result.source.contains("x_add__mutmut_1"), "Should have mutant variant");
        assert!(result.source.contains("x_add__mutmut_mutants"), "Should have lookup dict");
        assert!(!result.mutant_names.is_empty(), "Should produce mutant names");
    }

    #[test]
    fn test_mutate_file_no_mutations() {
        let source = "# just a comment\npass\n";
        let result = mutate_file(source, "empty");
        assert!(result.is_none(), "Should return None for files with no mutations");
    }

    #[test]
    fn test_mutate_file_preserves_imports() {
        let source = "import os\nimport sys\n\ndef add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "my_mod").unwrap();

        assert!(result.source.contains("import os"), "Should preserve original imports");
        assert!(result.source.contains("import sys"), "Should preserve original imports");
    }

    #[test]
    fn test_mutate_file_multiple_functions() {
        let source = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
        let result = mutate_file(source, "math_lib").unwrap();

        assert!(result.source.contains("x_add__mutmut_orig"), "Should have add original");
        assert!(result.source.contains("x_sub__mutmut_orig"), "Should have sub original");
        assert!(result.mutant_names.len() >= 2, "Should have mutants for both functions");
    }

    #[test]
    fn test_mutated_functions_not_duplicated() {
        let source = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
        let result = mutate_file(source, "m").unwrap();

        // The original function definitions should NOT appear in the output
        // (they're replaced by the trampoline arrangement)
        let add_count = result.source.matches("def add(").count();
        assert_eq!(add_count, 1, "Should have exactly one 'def add(' (the wrapper), got {add_count}");

        let sub_count = result.source.matches("def sub(").count();
        assert_eq!(sub_count, 1, "Should have exactly one 'def sub(' (the wrapper), got {sub_count}");
    }
}
