//! File-level codegen: take a Python source file and produce the fully
//! mutated version with all functions trampolined.

use crate::mutation::{collect_file_mutations, FunctionMutations};
use crate::trampoline::{generate_trampoline, trampoline_impl, TrampolineOutput};

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

    // Pre-generate all trampolines upfront.
    let trampolines: Vec<TrampolineOutput> = function_mutations
        .iter()
        .map(|fm| generate_trampoline(fm, module_name))
        .collect();

    let mut output = String::new();
    let mut all_mutant_names: Vec<String> = trampolines
        .iter()
        .flat_map(|tp| tp.mutant_keys.iter().cloned())
        .collect();
    // Keep insertion order stable for deterministic output.
    all_mutant_names.dedup();

    // Walk through source lines, stripping out functions that will be replaced
    // by trampoline arrangements. For class methods, emit the wrapper inline
    // (indented inside the class body). For top-level functions, the wrapper
    // is appended after the walk.
    let lines: Vec<&str> = source.lines().collect();

    // Python requires `from __future__` imports to be the very first statement.
    // Scan the leading lines and collect any blank lines, comments, module
    // docstrings, and `from __future__` imports so they can be emitted BEFORE
    // the trampoline preamble. We stop at the first line that is none of those.
    let mut skip_until = 0;
    let mut idx = 0;
    while idx < lines.len() {
        let t = lines[idx].trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("from __future__") {
            skip_until = idx + 1;
            idx += 1;
            continue;
        }
        // Handle module-level docstrings (single-line or multi-line).
        if t.starts_with("\"\"\"") || t.starts_with("'''") {
            let quote = if t.starts_with("\"\"\"") { "\"\"\"" } else { "'''" };
            let rest = &t[quote.len()..];
            if rest.contains(quote) {
                // Single-line docstring: opens and closes on the same line.
                skip_until = idx + 1;
                idx += 1;
            } else {
                // Multi-line docstring: scan forward until the closing delimiter.
                skip_until = idx + 1;
                idx += 1;
                while idx < lines.len() {
                    skip_until = idx + 1;
                    let close = lines[idx].trim();
                    idx += 1;
                    if close.contains(quote) {
                        break;
                    }
                }
            }
            continue;
        }
        break;
    }
    // Emit __future__ preamble lines before the trampoline runtime.
    for line in &lines[..skip_until] {
        output.push_str(line);
        output.push('\n');
    }

    // Now emit the trampoline implementation.
    output.push_str(trampoline_impl());
    output.push('\n');

    let mut i = skip_until;
    // Track which class we're currently inside: (class_name, class_indent_level).
    let mut current_class: Option<(String, usize)> = None;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        // Exit class context when we reach a non-empty line at or above the class indent.
        if let Some((_, class_indent)) = current_class.as_ref() {
            if !trimmed.is_empty() && indent <= *class_indent {
                current_class = None;
            }
        }

        // Detect class definitions.
        if trimmed.starts_with("class ") && trimmed.contains(':') {
            if let Some(class_name) = extract_class_name(trimmed) {
                current_class = Some((class_name.to_string(), indent));
            }
            output.push_str(line);
            output.push('\n');
            i += 1;
            continue;
        }

        // Check if this line starts a function definition we're mutating.
        let func_name = extract_func_name(trimmed);
        let class_key = current_class.as_ref().map(|(name, _)| name.as_str());

        if !func_name.is_empty() {
            if let Some(idx) = find_trampoline_idx(&function_mutations, class_key, func_name) {
                // Strip the entire function body.
                let func_indent = indent;
                i += 1;
                while i < lines.len() {
                    let next = lines[i];
                    let next_trimmed = next.trim_start();
                    let next_indent = next.len() - next_trimmed.len();
                    // Function ends when we hit a non-empty line at same or lesser indent.
                    if !next_trimmed.is_empty() && next_indent <= func_indent {
                        break;
                    }
                    i += 1;
                }

                // For class methods: emit the wrapper inline, indented to the method's level.
                // The original name is preserved so instance.method() keeps working.
                if class_key.is_some() {
                    let wrapper = &trampolines[idx].wrapper_code;
                    let indented = indent_code(wrapper, func_indent);
                    output.push_str(&indented);
                    output.push('\n');
                }
                // For top-level functions: wrapper is emitted after the walk (module level).
                continue;
            }
        }

        output.push_str(line);
        output.push('\n');
        i += 1;
    }

    // Append module-level code for ALL functions (mangled orig, variants, lookup dict).
    // These have globally-unique mangled names and are safe at module level.
    output.push('\n');
    for tp in &trampolines {
        output.push_str(&tp.module_code);
        output.push_str("\n\n");
    }

    // Append wrappers for TOP-LEVEL functions only.
    // Class method wrappers were already emitted inline during the walk.
    for (idx, fm) in function_mutations.iter().enumerate() {
        if fm.class_name.is_none() {
            output.push_str(&trampolines[idx].wrapper_code);
            output.push_str("\n\n");
        }
    }

    Some(MutatedFile {
        source: output,
        mutant_names: all_mutant_names,
    })
}

/// Find the index of the trampoline for (class_name, func_name) in function_mutations.
fn find_trampoline_idx(
    function_mutations: &[FunctionMutations],
    class_name: Option<&str>,
    func_name: &str,
) -> Option<usize> {
    function_mutations
        .iter()
        .position(|fm| fm.name == func_name && fm.class_name.as_deref() == class_name)
}

fn extract_class_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("class ")?;
    let name = rest.split(['(', ':']).next()?.trim();
    if name.is_empty() { None } else { Some(name) }
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

/// Indent every line of `code` by `indent` spaces.
/// Empty/whitespace-only lines are left blank (no trailing spaces).
fn indent_code(code: &str, indent: usize) -> String {
    let prefix = " ".repeat(indent);
    code.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mutate_simple_file() {
        let source = "def add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "simple_lib").unwrap();

        assert!(
            result.source.contains("import irradiate_harness"),
            "Should have harness import"
        );
        assert!(
            result.source.contains("x_add__mutmut_orig"),
            "Should have original renamed"
        );
        assert!(
            result.source.contains("x_add__mutmut_1"),
            "Should have mutant variant"
        );
        assert!(
            result.source.contains("x_add__mutmut_mutants"),
            "Should have lookup dict"
        );
        assert!(
            !result.mutant_names.is_empty(),
            "Should produce mutant names"
        );
    }

    #[test]
    fn test_mutate_file_no_mutations() {
        let source = "# just a comment\npass\n";
        let result = mutate_file(source, "empty");
        assert!(
            result.is_none(),
            "Should return None for files with no mutations"
        );
    }

    #[test]
    fn test_mutate_file_preserves_imports() {
        let source = "import os\nimport sys\n\ndef add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "my_mod").unwrap();

        assert!(
            result.source.contains("import os"),
            "Should preserve original imports"
        );
        assert!(
            result.source.contains("import sys"),
            "Should preserve original imports"
        );
    }

    #[test]
    fn test_mutate_file_multiple_functions() {
        let source = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
        let result = mutate_file(source, "math_lib").unwrap();

        assert!(
            result.source.contains("x_add__mutmut_orig"),
            "Should have add original"
        );
        assert!(
            result.source.contains("x_sub__mutmut_orig"),
            "Should have sub original"
        );
        assert!(
            result.mutant_names.len() >= 2,
            "Should have mutants for both functions"
        );
    }

    #[test]
    fn test_class_method_wrapper_stays_inside_class() {
        let source = "\
class Calculator:
    def add(self, a, b):
        return a + b
";
        let result = mutate_file(source, "calc").unwrap();

        // The wrapper `def add(self, a, b)` must be INSIDE the class body,
        // i.e. it must appear indented after `class Calculator:`.
        // If it's at module level, `Calculator().add(1, 2)` will fail because
        // the class has no `add` method.
        let lines: Vec<&str> = result.source.lines().collect();
        let class_line = lines
            .iter()
            .position(|l| l.contains("class Calculator"))
            .expect("class Calculator should exist in output");

        // Find the wrapper: `def add(self, a, b):` (NOT the mangled orig)
        let wrapper_line = lines
            .iter()
            .position(|l| {
                let trimmed = l.trim_start();
                trimmed.starts_with("def add(") && !trimmed.contains("mutmut")
            })
            .expect("wrapper def add( should exist in output");

        assert!(
            wrapper_line > class_line,
            "wrapper must appear after class definition"
        );

        // The wrapper must be indented (i.e. inside the class body)
        let wrapper_text = lines[wrapper_line];
        let indent = wrapper_text.len() - wrapper_text.trim_start().len();
        assert!(
            indent > 0,
            "wrapper 'def add(' should be indented (inside class body), but got: {:?}",
            wrapper_text
        );
    }

    #[test]
    fn test_class_method_init_stays_inside_class() {
        // Regression test for: trampolined __init__ ends up at module level,
        // causing `TypeError: ClassName() takes no arguments`
        let source = "\
class Finder:
    def __init__(self, path):
        self.path = path

    def search(self, query):
        return query in self.path
";
        let result = mutate_file(source, "finder").unwrap();

        // Parse the output: both __init__ and search wrappers must be inside the class
        let lines: Vec<&str> = result.source.lines().collect();
        let class_line = lines
            .iter()
            .position(|l| l.contains("class Finder"))
            .expect("class Finder should exist");

        // Find wrapper for __init__ (not the mangled orig)
        let init_wrapper = lines
            .iter()
            .position(|l| {
                let trimmed = l.trim_start();
                trimmed.starts_with("def __init__(") && !trimmed.contains("mutmut")
            })
            .expect("wrapper def __init__( should exist");

        assert!(
            init_wrapper > class_line,
            "__init__ wrapper must appear after class definition"
        );
        let init_text = lines[init_wrapper];
        let init_indent = init_text.len() - init_text.trim_start().len();
        assert!(
            init_indent > 0,
            "__init__ wrapper should be indented (inside class body), but got: {:?}",
            init_text
        );
    }

    #[test]
    fn test_mutated_functions_not_duplicated() {
        let source = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
        let result = mutate_file(source, "m").unwrap();

        // The original function definitions should NOT appear in the output
        // (they're replaced by the trampoline arrangement)
        let add_count = result.source.matches("def add(").count();
        assert_eq!(
            add_count, 1,
            "Should have exactly one 'def add(' (the wrapper), got {add_count}"
        );

        let sub_count = result.source.matches("def sub(").count();
        assert_eq!(
            sub_count, 1,
            "Should have exactly one 'def sub(' (the wrapper), got {sub_count}"
        );
    }

    // INV-1: class method wrapper is indented inside class body
    // INV-2: top-level function wrapper is at module level (indent 0)
    // INV-3: mangled orig/variants/dict always at module level

    #[test]
    fn test_top_level_wrapper_at_module_level() {
        let source = "def add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "m").unwrap();

        // The wrapper `def add(` must NOT be indented (indent == 0)
        let wrapper_line = result
            .source
            .lines()
            .find(|l| {
                let trimmed = l.trim_start();
                trimmed.starts_with("def add(") && !trimmed.contains("mutmut")
            })
            .expect("wrapper def add( should exist");
        let indent = wrapper_line.len() - wrapper_line.trim_start().len();
        assert_eq!(indent, 0, "top-level wrapper must be at indent 0, got: {wrapper_line:?}");
    }

    #[test]
    fn test_mangled_code_at_module_level() {
        let source = "\
class Calc:
    def add(self, a, b):
        return a + b
";
        let result = mutate_file(source, "m").unwrap();

        // The mangled orig function definition must appear at indent 0 (module level)
        let orig_line = result
            .source
            .lines()
            .find(|l| l.starts_with("def xǁCalcǁadd__mutmut_orig("))
            .expect("mangled orig def should exist at module level");
        let indent = orig_line.len() - orig_line.trim_start().len();
        assert_eq!(indent, 0, "mangled orig must be at module level, got: {orig_line:?}");
    }

    #[test]
    fn test_mixed_class_and_top_level() {
        // File with both a class method and a top-level function.
        // Both should be trampolined correctly.
        let source = "\
def compute(x):
    return x + 1

class Processor:
    def run(self, x):
        return x - 1
";
        let result = mutate_file(source, "mixed").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        // top-level `compute` wrapper must be at indent 0
        let compute_wrapper = lines
            .iter()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with("def compute(") && !t.contains("mutmut")
            })
            .expect("wrapper for compute should exist");
        let compute_indent = compute_wrapper.len() - compute_wrapper.trim_start().len();
        assert_eq!(compute_indent, 0, "compute wrapper must be at module level");

        // class method `run` wrapper must be indented inside Processor
        let class_pos = lines
            .iter()
            .position(|l| l.contains("class Processor"))
            .expect("class Processor should exist");
        let run_wrapper = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def run(") && !t.contains("mutmut")
            })
            .expect("wrapper for run should exist");
        assert!(run_wrapper > class_pos, "run wrapper must be after class definition");
        let run_text = lines[run_wrapper];
        let run_indent = run_text.len() - run_text.trim_start().len();
        assert!(run_indent > 0, "run wrapper must be indented inside Processor");
    }

    #[test]
    fn test_two_classes_same_method_name() {
        // Two classes both with a `process` method — each should get its own wrapper.
        let source = "\
class Alpha:
    def process(self, x):
        return x + 1

class Beta:
    def process(self, x):
        return x - 1
";
        let result = mutate_file(source, "dual").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let alpha_pos = lines.iter().position(|l| l.contains("class Alpha")).expect("class Alpha");
        let beta_pos = lines.iter().position(|l| l.contains("class Beta")).expect("class Beta");

        // Both classes should appear in the output
        assert!(alpha_pos < beta_pos, "Alpha before Beta");

        // Each class should have at least one indented `def process(` wrapper
        // Find all `def process(` wrappers (not mangled)
        let wrapper_positions: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                let t = l.trim_start();
                if t.starts_with("def process(") && !t.contains("mutmut") {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            wrapper_positions.len(),
            2,
            "Should have exactly 2 process wrappers (one per class), got {}: {result:?}",
            wrapper_positions.len(),
            result = result.source
        );

        // First wrapper should be between Alpha and Beta
        assert!(
            wrapper_positions[0] > alpha_pos && wrapper_positions[0] < beta_pos,
            "First process wrapper should be inside Alpha"
        );
        // Second wrapper should be after Beta
        assert!(
            wrapper_positions[1] > beta_pos,
            "Second process wrapper should be inside Beta"
        );

        // Both should be indented
        for &pos in &wrapper_positions {
            let text = lines[pos];
            let ind = text.len() - text.trim_start().len();
            assert!(ind > 0, "process wrapper at line {pos} should be indented, got: {text:?}");
        }
    }

    // --- __future__ import ordering tests (including docstring hoisting) ---

    #[test]
    fn test_single_line_docstring_before_future_import() {
        // INV-1: single-line docstring then from __future__ → both before preamble.
        let source =
            "\"\"\"Module docstring.\"\"\"\n\nfrom __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let doc_pos = lines
            .iter()
            .position(|l| l.starts_with("\"\"\"Module"))
            .expect("docstring must be present in output");
        let future_pos = lines
            .iter()
            .position(|l| l.starts_with("from __future__"))
            .expect("__future__ import must be present in output");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");

        assert!(doc_pos < future_pos, "docstring (line {doc_pos}) must come before __future__ (line {future_pos})");
        assert!(
            future_pos < preamble_pos,
            "from __future__ (line {future_pos}) must come before trampoline preamble (line {preamble_pos})"
        );
    }

    #[test]
    fn test_multiline_docstring_before_future_import() {
        // INV-2: multi-line docstring before from __future__ → both before preamble.
        let source = "\"\"\"Multi-line\ndocstring here.\n\"\"\"\n\nfrom __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let future_pos = lines
            .iter()
            .position(|l| l.starts_with("from __future__"))
            .expect("__future__ import must be present in output");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");
        // Docstring opener must appear somewhere before the preamble
        let doc_pos = lines
            .iter()
            .position(|l| l.starts_with("\"\"\"Multi"))
            .expect("docstring opener must be present in output");

        assert!(doc_pos < preamble_pos, "docstring opener (line {doc_pos}) must come before preamble (line {preamble_pos})");
        assert!(
            future_pos < preamble_pos,
            "from __future__ (line {future_pos}) must come before trampoline preamble (line {preamble_pos})"
        );
    }

    #[test]
    fn test_docstring_only_no_future_import() {
        // Docstring without __future__ import — docstring should stay before preamble.
        let source = "\"\"\"Just a docstring.\"\"\"\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let doc_pos = lines
            .iter()
            .position(|l| l.starts_with("\"\"\"Just"))
            .expect("docstring must be present in output");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");

        assert!(doc_pos < preamble_pos, "docstring (line {doc_pos}) must come before preamble (line {preamble_pos})");
    }

    #[test]
    fn test_future_import_before_preamble() {
        // INV-1: from __future__ import annotations must appear before the trampoline preamble.
        let source = "from __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let future_pos = result
            .source
            .lines()
            .position(|l| l.starts_with("from __future__"))
            .expect("__future__ import must be present in output");
        let preamble_pos = result
            .source
            .lines()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");
        assert!(
            future_pos < preamble_pos,
            "from __future__ (line {future_pos}) must come before trampoline preamble (line {preamble_pos})"
        );
    }

    #[test]
    fn test_leading_comment_and_future_import_before_preamble() {
        // INV-2: leading comments and blank lines before __future__ are preserved in their original position.
        let source = "# comment\nfrom __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let comment_pos = lines.iter().position(|l| *l == "# comment").expect("comment must be present");
        let future_pos = lines
            .iter()
            .position(|l| l.starts_with("from __future__"))
            .expect("__future__ import must be present");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");

        assert!(comment_pos < future_pos, "comment must appear before __future__");
        assert!(future_pos < preamble_pos, "from __future__ must appear before trampoline preamble");
    }

    #[test]
    fn test_no_future_import_preamble_first() {
        // INV-3: files without __future__ imports still have preamble first (existing behavior).
        let source = "import os\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod").unwrap();
        let preamble_pos = result
            .source
            .lines()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");
        // The very first non-empty line should be the preamble (or part of it),
        // and there must be no `from __future__` anywhere (none in source).
        assert!(!result.source.contains("from __future__"), "no __future__ in output when not in source");
        assert_eq!(preamble_pos, 0, "preamble must start at line 0 when no __future__ present");
    }

    #[test]
    fn test_class_all_methods_mutated_not_empty() {
        // When ALL methods are mutated, the class body shouldn't become empty.
        // The wrappers fill the body.
        let source = "\
class Single:
    def only(self, x):
        return x + 1
";
        let result = mutate_file(source, "single").unwrap();

        // class body should not be empty — the wrapper must be there
        let lines: Vec<&str> = result.source.lines().collect();
        let class_pos = lines.iter().position(|l| l.contains("class Single")).expect("class Single");

        // The very next non-empty line after `class Single:` must be the wrapper
        let first_body_line = lines[class_pos + 1..]
            .iter()
            .find(|l| !l.trim().is_empty())
            .expect("class body should not be empty");
        let t = first_body_line.trim_start();
        assert!(
            t.starts_with("def only(") && !t.contains("mutmut"),
            "First line of class body after stripping should be the wrapper, got: {first_body_line:?}"
        );
    }
}
