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

/// A source-patch mutation: a direct byte-range replacement in the original source file.
///
/// Used for functions with non-descriptor decorators (e.g., `@lru_cache`, `@app.route`)
/// that can't be trampolined. Instead of renaming/wrapping, the mutation is applied by
/// writing a patched copy of the source file and running tests in a subprocess.
#[derive(Debug, Clone)]
pub struct SourcePatchMutation {
    /// Absolute byte offset in the source file where the original text starts.
    pub file_byte_start: usize,
    /// Absolute byte offset one past the end of the original text.
    pub file_byte_end: usize,
    /// The original text to replace.
    pub original: String,
    /// The replacement text.
    pub replacement: String,
    /// Which operator produced this mutation.
    pub operator: &'static str,
    /// Function name as it appears in the source.
    pub function_name: String,
    /// Class name if this is a method.
    pub class_name: Option<String>,
    /// The complete source text of the function definition (for cache invalidation).
    pub function_source: String,
    /// Byte offset of the function definition start in the source file.
    pub fn_byte_offset: usize,
    /// 1-indexed start line of the decorated definition (including decorator lines).
    pub start_line: usize,
    /// 1-indexed end line of the function.
    pub end_line: usize,
}

/// Result of collecting all mutations from a source file.
pub struct FileCollectionResult {
    /// Mutations for functions that can be trampolined (descriptor-decorated or undecorated).
    pub trampoline: Vec<FunctionMutations>,
    /// Source-patch mutations for functions with non-descriptor decorators.
    pub source_patches: Vec<SourcePatchMutation>,
}

/// Re-export from `tree_sitter_mutation` so callers can use `mutation::collect_file_mutations`.
pub use crate::tree_sitter_mutation::collect_file_mutations;

/// Apply a single mutation to a function's source text.
pub fn apply_mutation(func_source: &str, mutation: &Mutation) -> String {
    format!(
        "{}{}{}",
        &func_source[..mutation.start],
        mutation.replacement,
        &func_source[mutation.end..]
    )
}

/// Convert a byte offset within source text to a 1-indexed (line, column) pair.
///
/// `line` is the 1-indexed line number; `column` is the 1-indexed byte column within that line.
/// Used to convert `fn_byte_offset + mutation.start` into a human-readable file position.
pub fn byte_offset_to_location(source: &str, byte_offset: usize) -> (usize, usize) {
    let prefix = &source[..byte_offset.min(source.len())];
    let line = prefix.matches('\n').count() + 1;
    let col = prefix.len() - prefix.rfind('\n').map(|p| p + 1).unwrap_or(0) + 1;
    (line, col)
}

/// Check whether source is syntactically valid Python (tree-sitter based).
/// Used by many test modules for parse-validity assertions.
#[cfg(test)]
fn parses_as_python(source: &str) -> bool {
    crate::tree_sitter_mutation::parse_python(source).is_some()
}

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
#[path = "mutation_tests.rs"]
mod tests;
