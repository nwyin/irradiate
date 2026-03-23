//! File-level codegen: take a Python source file and produce the fully
//! mutated version with all functions trampolined.

use crate::cache::MutantCacheDescriptor;
use crate::git_diff::DiffFilter;
use crate::mutation::{collect_file_mutations, FunctionMutations};
use crate::trampoline::{generate_trampoline, trampoline_impl, TrampolineOutput};
use std::path::Path;

/// Result of mutating a single Python source file.
#[derive(Debug)]
pub struct MutatedFile {
    /// The fully mutated source code.
    pub source: String,
    /// List of mutant keys (e.g., "module.x_func__irradiate_1").
    pub mutant_names: Vec<String>,
    /// Rich descriptors for each generated mutant.
    pub descriptors: Vec<MutantCacheDescriptor>,
}

/// Generate the mutated version of a Python source file.
///
/// When `diff_filter` is provided, only functions touched by the diff are mutated.
/// `diff_filter` is `(filter, file_rel_path)` where `file_rel_path` is the path
/// relative to the git repository root (used to look up the file in the diff).
///
/// Returns None if no mutations were found (after filtering).
pub fn mutate_file(
    source: &str,
    module_name: &str,
    diff_filter: Option<(&DiffFilter, &Path)>,
) -> Option<MutatedFile> {
    let mut function_mutations = collect_file_mutations(source);

    // Filter to only functions touched by the diff when in incremental mode.
    if let Some((filter, rel_path)) = diff_filter {
        function_mutations
            .retain(|fm| filter.function_is_touched(rel_path, fm.start_line, fm.end_line));
    }

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
            let quote = if t.starts_with("\"\"\"") {
                "\"\"\""
            } else {
                "'''"
            };
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
    // Track top-level trampolines emitted inline (both module_code + wrapper_code),
    // so the final append loop can skip them.
    let mut emitted_inline: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Buffer for decorator lines. When we encounter @decorator lines, we buffer them
    // instead of emitting immediately. If the following `def` is trampolined, we discard
    // the buffered decorators (the trampoline provides its own decorator_prefix).
    // If the `def` is not trampolined, we flush the buffer to output.
    let mut decorator_buffer: Vec<String> = Vec::new();

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

        // Buffer decorator lines — they might belong to a trampolined function.
        if trimmed.starts_with('@') {
            decorator_buffer.push(format!("{line}\n"));
            i += 1;
            continue;
        }

        // Detect class definitions.
        if trimmed.starts_with("class ") && trimmed.contains(':') {
            // Flush any buffered decorators (class decorators are passthrough).
            for dec_line in decorator_buffer.drain(..) {
                output.push_str(&dec_line);
            }
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
                // Discard buffered decorator lines — the trampoline provides its own.
                decorator_buffer.clear();

                // Strip the entire function signature + body.
                let func_indent = indent;

                // Count open parens on the `def` line to detect multi-line signatures.
                let mut paren_depth: i32 = 0;
                for ch in line.chars() {
                    match ch {
                        '(' => paren_depth += 1,
                        ')' => paren_depth -= 1,
                        _ => {}
                    }
                }

                // If paren_depth > 0, the signature continues on the following lines.
                // Advance past all continuation lines until the signature is closed.
                i += 1;
                while i < lines.len() && paren_depth > 0 {
                    for ch in lines[i].chars() {
                        match ch {
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            _ => {}
                        }
                    }
                    i += 1;
                }

                // Now strip the body: any lines at a deeper indent than func_indent.
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

                // For class methods: emit the decorator prefix (e.g. @property) + wrapper
                // inline, indented to the method's level, then module_code (orig, variants, dict).
                // This keeps the __class__ cell intact so super() works correctly.
                if class_key.is_some() {
                    let dec_prefix = &trampolines[idx].decorator_prefix;
                    if !dec_prefix.is_empty() {
                        output.push_str(&indent_code(dec_prefix.trim_end(), func_indent));
                        output.push('\n');
                    }
                    let wrapper = &trampolines[idx].wrapper_code;
                    let indented = indent_code(wrapper, func_indent);
                    output.push_str(&indented);
                    output.push('\n');
                    let indented_module = indent_code(&trampolines[idx].module_code, func_indent);
                    output.push_str(&indented_module);
                    output.push('\n');
                } else {
                    // For top-level functions: emit module_code (orig, variants, dict) FIRST,
                    // then decorator_prefix + wrapper_code, both inline at the original def position.
                    output.push_str(&trampolines[idx].module_code);
                    output.push('\n');
                    let dec_prefix = &trampolines[idx].decorator_prefix;
                    if !dec_prefix.is_empty() {
                        output.push_str(dec_prefix);
                    }
                    output.push_str(&trampolines[idx].wrapper_code);
                    output.push('\n');
                    emitted_inline.insert(idx);
                }
                continue;
            }
        }

        // Non-decorator, non-def line: flush any buffered decorators first.
        // This handles cases like `@decorator` followed by non-function code.
        for dec_line in decorator_buffer.drain(..) {
            output.push_str(&dec_line);
        }

        output.push_str(line);
        output.push('\n');
        i += 1;
    }

    // Append module_code for any top-level function NOT yet emitted inline.
    // In the normal case all top-level functions are in emitted_inline (they were
    // found during the walk and their module_code was emitted inline before the wrapper).
    // This loop exists as a fallback for any edge case where the walk missed a function.
    //
    // For class methods, both module_code and wrapper_code were already emitted
    // inside the class body during the walk above (to preserve the __class__ cell
    // so that super() works correctly).
    output.push('\n');
    for (idx, fm) in function_mutations.iter().enumerate() {
        if fm.class_name.is_none() && !emitted_inline.contains(&idx) {
            output.push_str(&trampolines[idx].module_code);
            output.push_str("\n\n");
        }
    }

    let descriptors: Vec<MutantCacheDescriptor> = function_mutations
        .iter()
        .zip(trampolines.iter())
        .flat_map(|(fm, tp)| {
            fm.mutations
                .iter()
                .zip(tp.mutant_keys.iter())
                .map(|(mutation, mutant_name)| MutantCacheDescriptor {
                    mutant_name: mutant_name.clone(),
                    function_source: fm.source.clone(),
                    operator: mutation.operator.to_string(),
                    start: mutation.start,
                    end: mutation.end,
                    original: mutation.original.clone(),
                    replacement: mutation.replacement.clone(),
                    source_file: module_name.to_string(),
                    fn_byte_offset: fm.byte_offset,
                    fn_start_line: fm.start_line,
                })
                .collect::<Vec<_>>()
        })
        .collect();

    Some(MutatedFile {
        source: output,
        mutant_names: all_mutant_names,
        descriptors,
    })
}

/// Convert an absolute byte offset in a source string to `(line, column)`, both 1-indexed.
///
/// `line` is the 1-indexed line number; `column` is the 1-indexed byte column within that line.
/// Used to convert `fn_byte_offset + mutation.start` into a human-readable file position.
pub fn byte_offset_to_location(source: &str, byte_offset: usize) -> (usize, usize) {
    let prefix = &source[..byte_offset.min(source.len())];
    let line = prefix.matches('\n').count() + 1;
    let col = prefix.len() - prefix.rfind('\n').map(|p| p + 1).unwrap_or(0) + 1;
    (line, col)
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
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
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
    use crate::mutation::collect_file_mutations;

    const SIMPLE_ADD: &str = "def add(a, b):\n    return a + b\n";
    const TWO_FUNCTIONS: &str = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
    const CLASS_WITH_METHOD: &str = "class Calculator:\n    def add(self, a, b):\n        return a + b\n";

    // --- byte_offset_to_location ---

    #[test]
    fn test_byte_offset_to_location_start_of_file() {
        // INV-2: offset 0 → (1, 1)
        assert_eq!(byte_offset_to_location("abc", 0), (1, 1));
    }

    #[test]
    fn test_byte_offset_to_location_start_of_second_line() {
        // INV-1: "line1\nline2\nline3" at offset 6 → (2, 1)
        assert_eq!(byte_offset_to_location("line1\nline2\nline3", 6), (2, 1));
    }

    #[test]
    fn test_byte_offset_to_location_mid_line() {
        // "ab\ncd" — offset 4 is 'c' + 1 = 'd' which is col 2 on line 2
        assert_eq!(byte_offset_to_location("ab\ncd", 4), (2, 2));
    }

    #[test]
    fn test_byte_offset_to_location_clamps_to_end() {
        // Offset past end must not panic; clamps to source.len()
        let source = "abc";
        let _ = byte_offset_to_location(source, 100);
    }

    // --- descriptor source_file and fn_byte_offset ---

    #[test]
    fn test_descriptor_source_file_is_module_name() {
        let source = SIMPLE_ADD;
        let result = mutate_file(source, "mymod.core", None).unwrap();
        for desc in &result.descriptors {
            assert_eq!(
                desc.source_file, "mymod.core",
                "source_file must be the module name"
            );
        }
    }

    #[test]
    fn test_descriptor_fn_byte_offset_matches_function_start() {
        // A file where the function does NOT start at byte 0 — there's a module-level line first.
        let source = "X = 1\n\ndef add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "mod", None).unwrap();

        // fn_byte_offset must be the byte position of `def` in source.
        let fn_pos = source.find("def add").expect("def must be present");
        for desc in &result.descriptors {
            assert_eq!(
                desc.fn_byte_offset, fn_pos,
                "fn_byte_offset must point at the function's def keyword"
            );
        }
    }

    #[test]
    fn test_absolute_mutation_position_is_correct() {
        // INV-3: fn_byte_offset + mutation.start gives the absolute byte position.
        let source = "X = 1\n\ndef add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];

        for mutation in &fm.mutations {
            let abs = fm.byte_offset + mutation.start;
            let original_slice = &source[abs..abs + mutation.end - mutation.start];
            assert_eq!(
                original_slice, mutation.original,
                "absolute position must index the original text in the file"
            );
        }
    }

    #[test]
    fn test_mutate_simple_file() {
        let source = SIMPLE_ADD;
        let result = mutate_file(source, "simple_lib", None).unwrap();

        assert!(
            result.source.contains("import irradiate_harness"),
            "Should have harness import"
        );
        assert!(
            result.source.contains("x_add__irradiate_orig"),
            "Should have original renamed"
        );
        assert!(
            result.source.contains("x_add__irradiate_1"),
            "Should have mutant variant"
        );
        assert!(
            result.source.contains("x_add__irradiate_mutants"),
            "Should have lookup dict"
        );
        assert!(
            !result.mutant_names.is_empty(),
            "Should produce mutant names"
        );
        assert_eq!(
            result.descriptors.len(),
            result.mutant_names.len(),
            "Should emit one cache descriptor per mutant"
        );
        assert_eq!(
            result.descriptors[0].mutant_name, result.mutant_names[0],
            "Descriptor keys must align with generated mutant names"
        );
        // tree-sitter collects return_statement mutations before recursing into binary_operator,
        // so the first descriptor may be return_value or statement_deletion rather than binop_swap.
        // Assert that a binop_swap descriptor exists somewhere (not necessarily first).
        let binop_desc = result.descriptors.iter().find(|d| d.operator == "binop_swap");
        assert!(binop_desc.is_some(), "Must have at least one binop_swap descriptor");
        // tree-sitter's function_definition node may not include the trailing newline that
        // libcst codegen adds. Strip trailing whitespace before comparing.
        assert_eq!(binop_desc.unwrap().function_source.trim_end(), source.trim_end());
    }

    #[test]
    fn test_mutate_file_no_mutations() {
        let source = "# just a comment\npass\n";
        let result = mutate_file(source, "empty", None);
        assert!(
            result.is_none(),
            "Should return None for files with no mutations"
        );
    }

    #[test]
    fn test_mutate_file_preserves_imports() {
        let source = "import os\nimport sys\n\ndef add(a, b):\n    return a + b\n";
        let result = mutate_file(source, "my_mod", None).unwrap();

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
        let source = TWO_FUNCTIONS;
        let result = mutate_file(source, "math_lib", None).unwrap();

        assert!(
            result.source.contains("x_add__irradiate_orig"),
            "Should have add original"
        );
        assert!(
            result.source.contains("x_sub__irradiate_orig"),
            "Should have sub original"
        );
        assert!(
            result.mutant_names.len() >= 2,
            "Should have mutants for both functions"
        );
    }

    #[test]
    fn test_class_method_wrapper_stays_inside_class() {
        let source = CLASS_WITH_METHOD;
        let result = mutate_file(source, "calc", None).unwrap();

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
        let result = mutate_file(source, "finder", None).unwrap();

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
        let source = TWO_FUNCTIONS;
        let result = mutate_file(source, "m", None).unwrap();

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
    // INV-3: top-level mangled orig/variants/dict at module level
    // INV-4: class method mangled orig/variants/dict inside class body (super() fix)

    #[test]
    fn test_top_level_wrapper_at_module_level() {
        let source = SIMPLE_ADD;
        let result = mutate_file(source, "m", None).unwrap();

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
        assert_eq!(
            indent, 0,
            "top-level wrapper must be at indent 0, got: {wrapper_line:?}"
        );
    }

    #[test]
    fn test_class_method_mangled_code_inside_class_body() {
        // INV-4: for class methods, mangled orig/variants/dict must be inside the class body.
        // Keeping them inside the class body preserves the __class__ cell so super() works.
        let source = "\
class Calc:
    def add(self, a, b):
        return a + b
";
        let result = mutate_file(source, "m", None).unwrap();

        // The mangled orig function definition must be indented (inside the class body),
        // NOT at module level (indent 0).
        let orig_line = result
            .source
            .lines()
            .find(|l| {
                l.trim_start()
                    .starts_with("def xǁCalcǁadd__irradiate_orig(")
            })
            .expect("mangled orig def should exist");
        let indent = orig_line.len() - orig_line.trim_start().len();
        assert!(
            indent > 0,
            "mangled orig must be indented (inside class body), got: {orig_line:?}"
        );

        // Also verify the lookup dict is indented inside the class body
        let dict_line = result
            .source
            .lines()
            .find(|l| l.trim_start().starts_with("xǁCalcǁadd__irradiate_mutants"))
            .expect("mangled mutants dict should exist");
        let dict_indent = dict_line.len() - dict_line.trim_start().len();
        assert!(
            dict_indent > 0,
            "mangled mutants dict must be indented (inside class body), got: {dict_line:?}"
        );
    }

    #[test]
    fn test_top_level_mangled_code_at_module_level() {
        // INV-3: for top-level functions, mangled orig/variants/dict stay at module level.
        let source = SIMPLE_ADD;
        let result = mutate_file(source, "m", None).unwrap();

        let orig_line = result
            .source
            .lines()
            .find(|l| l.starts_with("def x_add__irradiate_orig("))
            .expect("mangled orig def should exist at module level");
        let indent = orig_line.len() - orig_line.trim_start().len();
        assert_eq!(
            indent, 0,
            "top-level mangled orig must be at module level, got: {orig_line:?}"
        );
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
        let result = mutate_file(source, "mixed", None).unwrap();
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
        assert!(
            run_wrapper > class_pos,
            "run wrapper must be after class definition"
        );
        let run_text = lines[run_wrapper];
        let run_indent = run_text.len() - run_text.trim_start().len();
        assert!(
            run_indent > 0,
            "run wrapper must be indented inside Processor"
        );
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
        let result = mutate_file(source, "dual", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let alpha_pos = lines
            .iter()
            .position(|l| l.contains("class Alpha"))
            .expect("class Alpha");
        let beta_pos = lines
            .iter()
            .position(|l| l.contains("class Beta"))
            .expect("class Beta");

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
            assert!(
                ind > 0,
                "process wrapper at line {pos} should be indented, got: {text:?}"
            );
        }
    }

    // --- __future__ import ordering tests (including docstring hoisting) ---

    #[test]
    fn test_single_line_docstring_before_future_import() {
        // INV-1: single-line docstring then from __future__ → both before preamble.
        let source =
            "\"\"\"Module docstring.\"\"\"\n\nfrom __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod", None).unwrap();
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

        assert!(
            doc_pos < future_pos,
            "docstring (line {doc_pos}) must come before __future__ (line {future_pos})"
        );
        assert!(
            future_pos < preamble_pos,
            "from __future__ (line {future_pos}) must come before trampoline preamble (line {preamble_pos})"
        );
    }

    #[test]
    fn test_multiline_docstring_before_future_import() {
        // INV-2: multi-line docstring before from __future__ → both before preamble.
        let source = "\"\"\"Multi-line\ndocstring here.\n\"\"\"\n\nfrom __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod", None).unwrap();
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

        assert!(
            doc_pos < preamble_pos,
            "docstring opener (line {doc_pos}) must come before preamble (line {preamble_pos})"
        );
        assert!(
            future_pos < preamble_pos,
            "from __future__ (line {future_pos}) must come before trampoline preamble (line {preamble_pos})"
        );
    }

    #[test]
    fn test_docstring_only_no_future_import() {
        // Docstring without __future__ import — docstring should stay before preamble.
        let source = "\"\"\"Just a docstring.\"\"\"\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let doc_pos = lines
            .iter()
            .position(|l| l.starts_with("\"\"\"Just"))
            .expect("docstring must be present in output");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");

        assert!(
            doc_pos < preamble_pos,
            "docstring (line {doc_pos}) must come before preamble (line {preamble_pos})"
        );
    }

    // --- multi-line signature tests ---

    #[test]
    fn test_multiline_signature_class_method_no_orphan_lines() {
        // INV-1: multi-line class method signature is fully stripped — no orphan `)` or `-> ...` lines.
        let source = "\
class Markup:
    def format_map(
        self,
        mapping,
    ):
        return 1 + 2
";
        let result = mutate_file(source, "markup", None).unwrap();

        // The body `return 1 + 2` must appear exactly once (in the mangled orig).
        // If the body leaked as orphan code in the class, it would appear twice.
        let body_count = result.source.matches("return 1 + 2").count();
        assert_eq!(
            body_count, 1,
            "Body should appear exactly once (in mangled orig), got {body_count}. Orphan body leaked:\n{}",
            result.source
        );

        // The wrapper def format_map( should appear exactly once (the trampolined version)
        let wrapper_count = result.source.matches("def format_map(").count();
        assert_eq!(
            wrapper_count, 1,
            "Should have exactly one 'def format_map(' (the wrapper), got {wrapper_count}\n{}",
            result.source
        );

        // The wrapper should be indented inside the class
        let wrapper_line = result
            .source
            .lines()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with("def format_map(") && !t.contains("mutmut")
            })
            .expect("wrapper def format_map( should exist");
        let indent_len = wrapper_line.len() - wrapper_line.trim_start().len();
        assert!(indent_len > 0, "wrapper must be indented inside class body");
    }

    #[test]
    fn test_multiline_signature_top_level_function_no_orphan_lines() {
        // INV-2: multi-line top-level function signature is fully stripped.
        let source = "\
def build(
    x,
    y,
):
    return x + y
";
        let result = mutate_file(source, "top", None).unwrap();

        // No orphan standalone `)` lines
        for line in result.source.lines() {
            let trimmed = line.trim();
            assert!(
                trimmed != ")",
                "Orphan ')' found in output:\n{}",
                result.source
            );
        }

        // Exactly one `def build(` — the wrapper
        let count = result.source.matches("def build(").count();
        assert_eq!(
            count, 1,
            "Should have exactly one 'def build(' (the wrapper), got {count}\n{}",
            result.source
        );

        // Wrapper must be at module level (indent 0)
        let wrapper_line = result
            .source
            .lines()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with("def build(") && !t.contains("mutmut")
            })
            .expect("wrapper def build( should exist");
        let indent_len = wrapper_line.len() - wrapper_line.trim_start().len();
        assert_eq!(indent_len, 0, "top-level wrapper must be at module level");
    }

    #[test]
    fn test_multiline_signature_with_return_type_annotation() {
        // Multi-line signature with `-> ReturnType:` on the closing line (real-world pattern).
        // This is the markupsafe Markup.format_map() pattern.
        let source = "\
class Markup:
    def format_map(
        self,
        mapping,
    ) -> str:
        return 1 + 2
";
        let result = mutate_file(source, "markup", None).unwrap();

        // The body must appear exactly once (only in the mangled orig, not as orphan).
        let body_count = result.source.matches("return 1 + 2").count();
        assert_eq!(
            body_count, 1,
            "Body should appear exactly once (in mangled orig), got {body_count}. Orphan leaked:\n{}",
            result.source
        );

        // wrapper should exist, exactly once
        let count = result.source.matches("def format_map(").count();
        assert_eq!(
            count, 1,
            "Should have exactly one format_map wrapper\n{}",
            result.source
        );

        // The wrapper should be indented inside the class
        let wrapper_line = result
            .source
            .lines()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with("def format_map(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def format_map( should exist");
        let indent_len = wrapper_line.len() - wrapper_line.trim_start().len();
        assert!(
            indent_len > 0,
            "wrapper must be indented inside class body\n{}",
            result.source
        );

        // Mangled orig should also be inside the class body (indented) — INV-4 (super() fix)
        let orig_line = result
            .source
            .lines()
            .find(|l| {
                l.trim_start()
                    .starts_with("def xǁMarkupǁformat_map__irradiate_orig(")
            })
            .expect("mangled orig should exist");
        let orig_indent = orig_line.len() - orig_line.trim_start().len();
        assert!(
            orig_indent > 0,
            "mangled orig must be inside class body (super() fix)\n{}",
            result.source
        );
    }

    #[test]
    fn test_single_line_signature_regression() {
        // INV-3: Single-line signatures still work correctly after the fix.
        let source = "\
class Calc:
    def add(self, a, b):
        return a + b
";
        let result = mutate_file(source, "calc", None).unwrap();

        // Wrapper must be inside class (indented)
        let wrapper_line = result
            .source
            .lines()
            .find(|l| {
                let t = l.trim_start();
                t.starts_with("def add(") && !t.contains("mutmut")
            })
            .expect("wrapper def add( should exist");
        let indent_len = wrapper_line.len() - wrapper_line.trim_start().len();
        assert!(indent_len > 0, "wrapper must be indented inside class body");

        // Exactly one `def add(`
        let count = result.source.matches("def add(").count();
        assert_eq!(count, 1, "Exactly one def add( wrapper\n{}", result.source);
    }

    // --- __future__ import ordering tests ---

    #[test]
    fn test_future_import_before_preamble() {
        // INV-1: from __future__ import annotations must appear before the trampoline preamble.
        let source = "from __future__ import annotations\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod", None).unwrap();
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
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let comment_pos = lines
            .iter()
            .position(|l| *l == "# comment")
            .expect("comment must be present");
        let future_pos = lines
            .iter()
            .position(|l| l.starts_with("from __future__"))
            .expect("__future__ import must be present");
        let preamble_pos = lines
            .iter()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");

        assert!(
            comment_pos < future_pos,
            "comment must appear before __future__"
        );
        assert!(
            future_pos < preamble_pos,
            "from __future__ must appear before trampoline preamble"
        );
    }

    #[test]
    fn test_no_future_import_preamble_first() {
        // INV-3: files without __future__ imports still have preamble first (existing behavior).
        let source = "import os\n\ndef foo():\n    return 1\n";
        let result = mutate_file(source, "mod", None).unwrap();
        let preamble_pos = result
            .source
            .lines()
            .position(|l| l.contains("import irradiate_harness"))
            .expect("trampoline preamble must be present");
        // The very first non-empty line should be the preamble (or part of it),
        // and there must be no `from __future__` anywhere (none in source).
        assert!(
            !result.source.contains("from __future__"),
            "no __future__ in output when not in source"
        );
        assert_eq!(
            preamble_pos, 0,
            "preamble must start at line 0 when no __future__ present"
        );
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
        let result = mutate_file(source, "single", None).unwrap();

        // class body should not be empty — the wrapper must be there
        let lines: Vec<&str> = result.source.lines().collect();
        let class_pos = lines
            .iter()
            .position(|l| l.contains("class Single"))
            .expect("class Single");

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

    // ─────────────────────────────────────────────────────────────────
    // INV: wrapper must precede module-level calls (NameError regression)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_wrapper_precedes_module_level_call() {
        // INV-1: When a module-level statement calls a trampolined function,
        // the wrapper `def` must appear BEFORE that call in the output.
        // Regression: before the fix, the wrapper was appended at EOF but the
        // call was emitted inline — causing NameError at import time.
        let source = "\
def make_value(x):
    return x + 1

RESULT = make_value(42)
";
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let wrapper_pos = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def make_value(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def make_value( should exist");

        let call_pos = lines
            .iter()
            .position(|l| l.contains("RESULT = make_value("))
            .expect("module-level call should be preserved");

        assert!(
            wrapper_pos < call_pos,
            "wrapper (line {wrapper_pos}) must appear before module-level call (line {call_pos})\n{}",
            result.source
        );
    }

    #[test]
    fn test_multiple_wrappers_precede_their_calls() {
        // INV-2: Multiple trampolined functions, each called at module level —
        // every wrapper must appear before its respective call.
        let source = "\
def make_a(x):
    return x + 1

def make_b(x):
    return x - 1

A = make_a(10)
B = make_b(20)
";
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let wrapper_a = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def make_a(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def make_a( should exist");

        let call_a = lines
            .iter()
            .position(|l| l.contains("A = make_a("))
            .expect("call to make_a should be preserved");

        let wrapper_b = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def make_b(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def make_b( should exist");

        let call_b = lines
            .iter()
            .position(|l| l.contains("B = make_b("))
            .expect("call to make_b should be preserved");

        assert!(
            wrapper_a < call_a,
            "make_a wrapper (line {wrapper_a}) must be before call (line {call_a})\n{}",
            result.source
        );
        assert!(
            wrapper_b < call_b,
            "make_b wrapper (line {wrapper_b}) must be before call (line {call_b})\n{}",
            result.source
        );
    }

    #[test]
    fn test_wrapper_immediately_before_call_no_gap() {
        // INV-3: Wrapper is emitted inline even when the module-level call
        // appears immediately after the function definition (no other code between them).
        let source = "\
def factory(n):
    return n * 2
X = factory(5)
";
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let wrapper_pos = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def factory(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def factory( should exist");

        let call_pos = lines
            .iter()
            .position(|l| l.contains("X = factory("))
            .expect("module-level call to factory should be preserved");

        assert!(
            wrapper_pos < call_pos,
            "wrapper (line {wrapper_pos}) must be before call (line {call_pos})\n{}",
            result.source
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // INV-1 / INV-2: module_code must precede wrapper_code for top-level functions
    // (regression for NameError when module-level call executes at import time)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_module_code_precedes_wrapper_top_level() {
        // INV-2: For a top-level function, mangled orig (module_code) must be
        // defined BEFORE the wrapper (wrapper_code) that references it.
        // This ensures that even if nothing calls the function at module level,
        // the output is always in the correct order.
        let source = SIMPLE_ADD;
        let result = mutate_file(source, "m", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let orig_pos = lines
            .iter()
            .position(|l| l.starts_with("def x_add__irradiate_orig("))
            .expect("mangled orig must be defined");

        let wrapper_pos = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def add(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def add( should exist");

        assert!(
            orig_pos < wrapper_pos,
            "mangled orig (line {orig_pos}) must appear before wrapper (line {wrapper_pos})\n{}",
            result.source
        );
    }

    #[test]
    fn test_mangled_orig_defined_before_module_level_call() {
        // INV-1: Full chain: mangled orig → wrapper → module-level call.
        // Before this fix, module_code was deferred to EOF but the wrapper was
        // emitted inline. When the module-level call executed the wrapper body,
        // x_func__irradiate_orig was not yet defined → NameError.
        let source = "\
def make_cached_stream_func(stream):
    return stream + 1

CACHED = make_cached_stream_func(42)
";
        let result = mutate_file(source, "mod", None).unwrap();
        let lines: Vec<&str> = result.source.lines().collect();

        let orig_pos = lines
            .iter()
            .position(|l| l.starts_with("def x_make_cached_stream_func__irradiate_orig("))
            .expect("mangled orig must be defined");

        let wrapper_pos = lines
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("def make_cached_stream_func(") && !t.contains("irradiate_orig")
            })
            .expect("wrapper def make_cached_stream_func( should exist");

        let call_pos = lines
            .iter()
            .position(|l| l.contains("CACHED = make_cached_stream_func("))
            .expect("module-level call should be preserved");

        assert!(
            orig_pos < wrapper_pos,
            "mangled orig (line {orig_pos}) must appear before wrapper (line {wrapper_pos})\n{}",
            result.source
        );
        assert!(
            wrapper_pos < call_pos,
            "wrapper (line {wrapper_pos}) must appear before module-level call (line {call_pos})\n{}",
            result.source
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Descriptor decorator tests
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_classmethod_wrapper_has_decorator() {
        let source = concat!(
            "class Foo:\n",
            "    @classmethod\n",
            "    def make(cls, n):\n",
            "        return n + 1\n",
        );
        let result = mutate_file(source, "test_mod", None).expect("should produce mutations");
        assert!(
            result.source.contains("@classmethod"),
            "output must contain @classmethod decorator on wrapper;\n{}",
            result.source
        );
        assert!(
            result.source.contains("cls."),
            "output must use cls. prefix for dispatch;\n{}",
            result.source
        );
    }

    #[test]
    fn test_staticmethod_wrapper_has_decorator() {
        let source = concat!(
            "class Foo:\n",
            "    @staticmethod\n",
            "    def helper(x):\n",
            "        return x + 1\n",
        );
        let result = mutate_file(source, "test_mod", None).expect("should produce mutations");
        assert!(
            result.source.contains("@staticmethod"),
            "output must contain @staticmethod decorator on wrapper;\n{}",
            result.source
        );
    }

    #[test]
    fn test_property_wrapper_has_decorator() {
        let source = concat!(
            "class Foo:\n",
            "    @property\n",
            "    def name(self):\n",
            "        return self._name\n",
        );
        let result = mutate_file(source, "test_mod", None).expect("should produce mutations");
        assert!(
            result.source.contains("@property"),
            "output must contain @property decorator on wrapper;\n{}",
            result.source
        );
    }

    #[test]
    fn test_non_descriptor_decorator_still_skipped() {
        let source = concat!(
            "class Foo:\n",
            "    @cache\n",
            "    def compute(self):\n",
            "        return 1 + 2\n",
            "\n",
            "    def plain(self, v):\n",
            "        return v + 1\n",
        );
        let result = mutate_file(source, "test_mod", None).expect("should produce mutations from plain");
        // @cache method should not be trampolined.
        assert!(
            !result.source.contains("compute__irradiate"),
            "non-descriptor decorated method must not be trampolined;\n{}",
            result.source
        );
        // plain method should be trampolined.
        assert!(
            result.source.contains("plain__irradiate"),
            "undecorated method must be trampolined;\n{}",
            result.source
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Property-based tests (proptest)
    // ─────────────────────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Check whether python3 is available on this machine.
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Feed `source` to `python3 -c "compile(stdin, ...)"` and return true if
    /// the source is syntactically valid Python.
    fn is_valid_python(source: &str) -> bool {
        use std::io::Write;
        let mut child = match std::process::Command::new("python3")
            .args([
                "-c",
                "import sys; compile(sys.stdin.read(), 'test.py', 'exec')",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(source.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    }

    proptest! {
        // Target 1, P1: mutate_file never panics on arbitrary string input.
        #[test]
        fn prop_mutate_file_no_panic(source in ".*") {
            let _ = mutate_file(&source, "test_mod", None);
        }

        // Target 1, P3: `from __future__` appears before the trampoline preamble.
        #[test]
        fn prop_mutate_file_future_before_preamble(
            has_future in any::<bool>(),
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute")],
            a    in prop_oneof![Just("a"),   Just("x")],
            b    in prop_oneof![Just("b"),   Just("y")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            let body = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            let source = if has_future {
                format!("from __future__ import annotations\n\n{body}")
            } else {
                body
            };
            if let Some(result) = mutate_file(&source, "test_mod", None) {
                if result.source.contains("from __future__") {
                    let lines: Vec<&str> = result.source.lines().collect();
                    let future_pos = lines
                        .iter()
                        .position(|l| l.starts_with("from __future__"))
                        .unwrap();
                    let preamble_pos = lines
                        .iter()
                        .position(|l| l.contains("import irradiate_harness"))
                        .unwrap();
                    prop_assert!(
                        future_pos < preamble_pos,
                        "from __future__ (line {future_pos}) must precede preamble (line {preamble_pos})"
                    );
                }
            }
        }

        // Target 1, P4: all mutant_names follow "module.mangled__irradiate_N" format.
        #[test]
        fn prop_mutate_file_mutant_names_wellformed(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute"), Just("process")],
            a    in prop_oneof![Just("a"),   Just("x"),   Just("lhs")],
            b    in prop_oneof![Just("b"),   Just("y"),   Just("rhs")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            if let Some(result) = mutate_file(&source, "test_mod", None) {
                for name in &result.mutant_names {
                    prop_assert!(name.starts_with("test_mod."), "must be module-qualified: {name}");
                    prop_assert!(name.contains("__irradiate_"), "must have __irradiate_: {name}");
                    let num = name.rsplit("__irradiate_").next().unwrap_or("");
                    prop_assert!(
                        !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
                        "variant number must be all digits: {name}"
                    );
                }
            }
        }

        // Target 1, P5: every mutated function has a trampoline wrapper `def name(` in the output.
        #[test]
        fn prop_mutate_file_mutated_functions_have_wrappers(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute")],
            a    in prop_oneof![Just("a"),   Just("x")],
            b    in prop_oneof![Just("b"),   Just("y")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            use crate::mutation::collect_file_mutations;
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            let fms = collect_file_mutations(&source);
            if let Some(result) = mutate_file(&source, "test_mod", None) {
                for fm in &fms {
                    let def_pattern = format!("def {}(", fm.name);
                    prop_assert!(
                        result.source.contains(&def_pattern),
                        "output must contain wrapper 'def {0}(' but got:\n{1}",
                        fm.name,
                        result.source
                    );
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        // Target 1, P2: mutate_file output is parseable Python (skipped if python3 unavailable).
        #[test]
        fn prop_mutate_file_output_parseable_python(
            func in prop_oneof![
                Just("foo"), Just("bar"), Just("compute"), Just("process"), Just("eval_val")
            ],
            a   in prop_oneof![Just("a"), Just("x"), Just("lhs"), Just("val")],
            b   in prop_oneof![Just("b"), Just("y"), Just("rhs"), Just("other")],
            op  in prop_oneof![Just("+"), Just("-"), Just("*"), Just("//")],
            has_future in any::<bool>(),
        ) {
            if !python3_available() {
                return Ok(());
            }
            let body = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            let source = if has_future {
                format!("from __future__ import annotations\n\n{body}")
            } else {
                body
            };
            if let Some(result) = mutate_file(&source, "test_mod", None) {
                prop_assert!(
                    is_valid_python(&result.source),
                    "output not parseable Python for input:\n{source}\noutput:\n{}",
                    result.source
                );
            }
        }
    }
}
